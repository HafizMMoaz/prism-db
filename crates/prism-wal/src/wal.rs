//! The [`Wal`]: open, append, group-commit flush, segment rotation, and replay.
//!
//! See `docs/components/wal.md`. The append path serializes records into the
//! active segment under a short-held mutex and assigns each an LSN. Durability
//! is on [`Wal::flush_through`], which coalesces concurrent committers: the
//! first to acquire the flush lock fsyncs the segment for everyone waiting
//! behind it. (The background-writer-thread design in the component doc is a
//! latency optimization; this coalescing flush has the same group-commit
//! semantics and is a documented follow-up to convert.)

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Lsn;
use crate::error::{Result, WalError};
use crate::record::{self, LogRecord};
use crate::segment::{self, SEGMENT_HEADER_SIZE, SegmentHeader};

/// How aggressively the WAL forces data to the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// fsync on flush and segment rotation (durable; the production setting).
    Fsync,
    /// No fsync. Faster, **not durable** — for tests only.
    None,
}

/// WAL configuration.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Fixed size of each segment file, in bytes.
    pub segment_size: u32,
    /// Durability mode.
    pub sync_mode: SyncMode,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            segment_size: 16 * 1024 * 1024,
            sync_mode: SyncMode::Fsync,
        }
    }
}

struct Inner {
    dir: PathBuf,
    segment_size: u32,
    sync_mode: SyncMode,
    active_id: u64,
    active_offset: u32,
    active_file: File,
    last_lsn: u64,
}

impl Inner {
    fn rotate(&mut self) -> Result<()> {
        let sync = matches!(self.sync_mode, SyncMode::Fsync);
        if sync {
            self.active_file.sync_data()?;
        }
        let new_id = self.active_id + 1;
        let file = create_segment(&self.dir, new_id, self.active_id, self.segment_size, sync)?;
        self.active_id = new_id;
        self.active_offset = SEGMENT_HEADER_SIZE as u32;
        self.active_file = file;
        Ok(())
    }
}

/// The write-ahead log.
pub struct Wal {
    inner: Mutex<Inner>,
    flush_lock: Mutex<()>,
    durable_lsn: AtomicU64,
}

impl Wal {
    /// Open (or create) a WAL in directory `dir`.
    ///
    /// On reopen, the active (highest-id) segment is scanned to find the write
    /// position and the durable high-water mark; a torn trailing record is
    /// discarded (the next append overwrites it).
    pub fn open(dir: &Path, config: Config) -> Result<Self> {
        std::fs::create_dir_all(dir)?;

        let mut ids = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                if let Some(id) = segment::parse_segment_id(name) {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();

        let sync = matches!(config.sync_mode, SyncMode::Fsync);
        let inner = if let Some(&max) = ids.last() {
            let path = dir.join(segment::segment_file_name(max));
            let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let header = SegmentHeader::decode(&bytes)?;
            let (active_offset, last_lsn) = scan_active(&bytes);
            Inner {
                dir: dir.to_path_buf(),
                segment_size: header.segment_size,
                sync_mode: config.sync_mode,
                active_id: max,
                active_offset,
                active_file: file,
                last_lsn,
            }
        } else {
            let file = create_segment(dir, 0, 0, config.segment_size, sync)?;
            Inner {
                dir: dir.to_path_buf(),
                segment_size: config.segment_size,
                sync_mode: config.sync_mode,
                active_id: 0,
                active_offset: SEGMENT_HEADER_SIZE as u32,
                active_file: file,
                last_lsn: 0,
            }
        };

        let durable = inner.last_lsn;
        Ok(Self {
            inner: Mutex::new(inner),
            flush_lock: Mutex::new(()),
            durable_lsn: AtomicU64::new(durable),
        })
    }

    /// Append a record, returning its assigned LSN. Not durable until a
    /// subsequent [`Self::flush_through`].
    pub fn append(&self, record: LogRecord) -> Result<Lsn> {
        let mut inner = self.inner.lock().expect("wal inner poisoned");

        let mut body = Vec::new();
        let rtype = record::encode_body(&record.payload, &mut body);
        let total = record::RECORD_HEADER_SIZE + body.len() + record::RECORD_CRC_SIZE;

        let usable = inner.segment_size as usize - SEGMENT_HEADER_SIZE;
        if total > usable {
            return Err(WalError::RecordTooLarge {
                size: total,
                max: usable,
            });
        }
        if inner.active_offset as usize + total > inner.segment_size as usize {
            inner.rotate()?;
        }

        let lsn = Lsn::from_parts(inner.active_id as u32, inner.active_offset);
        let frame = record::assemble_frame(lsn, record.txn_id, record.prev_lsn, rtype, &body);
        let offset = inner.active_offset as u64;
        inner.active_file.seek(SeekFrom::Start(offset))?;
        inner.active_file.write_all(&frame)?;
        inner.active_offset += total as u32;
        inner.last_lsn = lsn.as_u64();
        Ok(lsn)
    }

    /// Block until every record with LSN `<= up_to` is durable. Cheap if already
    /// durable; otherwise one fsync serves all waiting committers.
    pub fn flush_through(&self, up_to: Lsn) -> Result<()> {
        if self.durable_lsn.load(Ordering::SeqCst) >= up_to.as_u64() {
            return Ok(());
        }
        let _flush = self.flush_lock.lock().expect("wal flush lock poisoned");
        if self.durable_lsn.load(Ordering::SeqCst) >= up_to.as_u64() {
            return Ok(());
        }
        let (target, clone, sync) = {
            let inner = self.inner.lock().expect("wal inner poisoned");
            (
                inner.last_lsn,
                inner.active_file.try_clone()?,
                matches!(inner.sync_mode, SyncMode::Fsync),
            )
        };
        if sync {
            clone.sync_data()?;
        }
        self.durable_lsn.fetch_max(target, Ordering::SeqCst);
        Ok(())
    }

    /// The current durable LSN: records up to and including it are on disk.
    pub fn durable_lsn(&self) -> Lsn {
        Lsn(self.durable_lsn.load(Ordering::SeqCst))
    }

    /// The LSN of the most recently appended record (durable or not).
    pub fn last_lsn(&self) -> Lsn {
        Lsn(self.inner.lock().expect("wal inner poisoned").last_lsn)
    }

    /// Iterate records starting at `from`, in LSN order, across segments.
    ///
    /// Stops at the first torn record (CRC mismatch) or the end of the written
    /// log. Single-threaded by contract; used by recovery.
    pub fn replay(&self, from: Lsn) -> Replay {
        let dir = self.inner.lock().expect("wal inner poisoned").dir.clone();
        let offset = from.offset().max(SEGMENT_HEADER_SIZE as u32);
        Replay {
            dir,
            seg_id: from.segment_id() as u64,
            offset,
            seg: None,
            done: false,
        }
    }
}

/// Iterator over WAL records produced by [`Wal::replay`].
pub struct Replay {
    dir: PathBuf,
    seg_id: u64,
    offset: u32,
    seg: Option<(u64, Vec<u8>)>,
    done: bool,
}

impl Iterator for Replay {
    type Item = Result<(Lsn, LogRecord)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let need_load = !matches!(&self.seg, Some((id, _)) if *id == self.seg_id);
            if need_load {
                let path = self.dir.join(segment::segment_file_name(self.seg_id));
                match File::open(&path) {
                    Ok(mut f) => {
                        let mut bytes = Vec::new();
                        if let Err(e) = f.read_to_end(&mut bytes) {
                            self.done = true;
                            return Some(Err(WalError::Io(e)));
                        }
                        self.seg = Some((self.seg_id, bytes));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
                        self.done = true;
                        return None;
                    }
                    Err(e) => {
                        self.done = true;
                        return Some(Err(WalError::Io(e)));
                    }
                }
            }

            let bytes = &self.seg.as_ref().expect("segment loaded").1;
            let off = self.offset as usize;

            // Not enough room for a header, or an empty (zeroed) slot: this
            // segment is exhausted — advance to the next one.
            if off + record::RECORD_HEADER_SIZE > bytes.len() || u64_le(bytes, off) == 0 {
                self.seg_id += 1;
                self.offset = SEGMENT_HEADER_SIZE as u32;
                self.seg = None;
                continue;
            }

            let body_len = u32_le(bytes, off + 8) as usize;
            let total = record::RECORD_HEADER_SIZE + body_len + record::RECORD_CRC_SIZE;
            if off + total > bytes.len() {
                self.done = true; // incomplete trailing record = end of log
                return None;
            }

            return match record::decode_record(&bytes[off..off + total]) {
                Ok((lsn, rec, consumed)) => {
                    self.offset += consumed as u32;
                    Some(Ok((lsn, rec)))
                }
                Err(WalError::CrcMismatch) => {
                    self.done = true; // torn write = end of log
                    None
                }
                Err(e) => {
                    self.done = true;
                    Some(Err(e))
                }
            };
        }
    }
}

fn create_segment(
    dir: &Path,
    id: u64,
    prev_id: u64,
    segment_size: u32,
    sync: bool,
) -> Result<File> {
    let path = dir.join(segment::segment_file_name(id));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.set_len(segment_size as u64)?;
    let header = SegmentHeader {
        format_version: segment::WAL_FORMAT_VERSION,
        segment_size,
        segment_id: id,
        first_lsn: Lsn::from_parts(id as u32, SEGMENT_HEADER_SIZE as u32).as_u64(),
        created_at_micros: now_micros(),
        prev_segment_id: prev_id,
    }
    .encode();
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header)?;
    if sync {
        file.sync_data()?;
        sync_dir(dir);
    }
    Ok(file)
}

/// Scan the active segment to find the next write offset and the last valid LSN.
/// Stops at the first zeroed slot, incomplete frame, or CRC failure.
fn scan_active(bytes: &[u8]) -> (u32, u64) {
    let mut off = SEGMENT_HEADER_SIZE;
    let mut last = 0u64;
    loop {
        if off + record::RECORD_HEADER_SIZE > bytes.len() {
            break;
        }
        let lsn = u64_le(bytes, off);
        if lsn == 0 {
            break;
        }
        let body_len = u32_le(bytes, off + 8) as usize;
        let total = record::RECORD_HEADER_SIZE + body_len + record::RECORD_CRC_SIZE;
        if off + total > bytes.len() {
            break;
        }
        match record::decode_record(&bytes[off..off + total]) {
            Ok(_) => {
                last = lsn;
                off += total;
            }
            Err(_) => break,
        }
    }
    (off as u32, last)
}

fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

fn u32_le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_le(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::RecordPayload;
    use prism_storage::PageId;

    /// A self-deleting temporary directory.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let nanos = now_micros();
            let mut p = std::env::temp_dir();
            p.push(format!(
                "prism-wal-{tag}-{}-{n}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn insert(txn: u64, page: u64, slot: u16, n: usize) -> LogRecord {
        LogRecord::txn(
            txn,
            Lsn::ZERO,
            RecordPayload::Insert {
                page_id: PageId(page),
                slot_id: slot,
                after_image: vec![txn as u8; n],
            },
        )
    }

    fn small_config() -> Config {
        Config {
            segment_size: 64 * 1024,
            sync_mode: SyncMode::Fsync,
        }
    }

    #[test]
    fn append_flush_replay_roundtrip() {
        let dir = TempDir::new("rt");
        let wal = Wal::open(dir.path(), small_config()).unwrap();
        let l1 = wal.append(insert(1, 10, 0, 20)).unwrap();
        let l2 = wal.append(insert(2, 11, 1, 30)).unwrap();
        wal.flush_through(l2).unwrap();
        assert!(wal.durable_lsn() >= l2);

        let got: Vec<_> = wal
            .replay(Lsn::from_parts(0, SEGMENT_HEADER_SIZE as u32))
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, l1);
        assert_eq!(got[1].0, l2);
        assert_eq!(got[0].1, insert(1, 10, 0, 20));
    }

    #[test]
    fn durable_survives_reopen_and_lsns_continue() {
        let dir = TempDir::new("reopen");
        let (l1, l2) = {
            let wal = Wal::open(dir.path(), small_config()).unwrap();
            let l1 = wal.append(insert(1, 10, 0, 16)).unwrap();
            let l2 = wal.append(insert(2, 10, 1, 16)).unwrap();
            wal.flush_through(l2).unwrap();
            (l1, l2)
        };

        let wal = Wal::open(dir.path(), small_config()).unwrap();
        assert_eq!(wal.durable_lsn(), l2);
        // New appends continue after the recovered position.
        let l3 = wal.append(insert(3, 10, 2, 16)).unwrap();
        assert!(l3 > l2);

        let lsns: Vec<_> = wal
            .replay(Lsn::from_parts(0, SEGMENT_HEADER_SIZE as u32))
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(lsns, vec![l1, l2, l3]);
    }

    #[test]
    fn rotation_spans_multiple_segments() {
        let dir = TempDir::new("rotate");
        // Tiny segments so a handful of records force several rotations.
        let cfg = Config {
            segment_size: 256,
            sync_mode: SyncMode::Fsync,
        };
        let wal = Wal::open(dir.path(), cfg).unwrap();
        let mut lsns = Vec::new();
        for i in 0..20u64 {
            lsns.push(wal.append(insert(i, i, 0, 8)).unwrap());
        }
        wal.flush_through(*lsns.last().unwrap()).unwrap();

        // Records landed in more than one segment.
        let segments: std::collections::BTreeSet<u32> =
            lsns.iter().map(|l| l.segment_id()).collect();
        assert!(segments.len() > 1, "expected rotation, got {segments:?}");

        let replayed: Vec<_> = wal
            .replay(Lsn::from_parts(0, SEGMENT_HEADER_SIZE as u32))
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(replayed, lsns);
    }

    #[test]
    fn torn_write_stops_replay() {
        let dir = TempDir::new("torn");
        let (l1, l2) = {
            let wal = Wal::open(dir.path(), small_config()).unwrap();
            let l1 = wal.append(insert(1, 10, 0, 16)).unwrap();
            let l2 = wal.append(insert(2, 10, 1, 16)).unwrap();
            wal.flush_through(l2).unwrap();
            (l1, l2)
        };

        // Corrupt a byte inside the second record's frame, then reopen+replay.
        let path = dir.path().join(segment::segment_file_name(0));
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[l2.offset() as usize + record::RECORD_HEADER_SIZE] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let wal = Wal::open(dir.path(), small_config()).unwrap();
        let got: Vec<_> = wal
            .replay(Lsn::from_parts(0, SEGMENT_HEADER_SIZE as u32))
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(got, vec![l1], "replay must stop at the torn record");
    }
}
