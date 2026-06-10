//! `prism-fsck` — an offline integrity checker for a Prism database directory.
//!
//! Reads only the on-disk formats (no live engine, no recovery, no mutation) and
//! validates them:
//! - **Page checksums**: every allocated heap/index page's stored checksum
//!   matches its contents (catches torn/bit-rotted pages). Page 0 is noted if it
//!   is a database header (a reserved-but-unused format today).
//! - **WAL integrity**: every segment header decodes and every record's CRC is
//!   intact; a CRC failure is flagged as corruption, while a truncated final
//!   record (a crash mid-write) is just a warning.
//!
//! Per `docs/architecture/module-layout.md` this depends only on `prism-storage`
//! and `prism-wal`. Deeper semantic checks — MVCC version chains, heap↔index
//! consistency — need the engine layer and are a follow-up.

use std::fmt;
use std::path::Path;

use prism_storage::{DbHeader, PAGE_SIZE, PageType, checksum};
use prism_wal::WalError;
use prism_wal::record::{RECORD_HEADER_SIZE, decode_record};
use prism_wal::segment::{SEGMENT_HEADER_SIZE, SegmentHeader, parse_segment_id};

// Page-header field offsets (the shared 32-byte Prism page header).
const OFF_CHECKSUM: usize = 8;
const OFF_PAGE_TYPE: usize = 10;

/// The outcome of checking a database directory.
#[derive(Default, Debug)]
pub struct Report {
    /// Whether `heap.db` page 0 is a database header (vs a data page — the
    /// current store reserves no header page, so this is informational).
    pub header_present: bool,
    /// Total pages in `heap.db`.
    pub pages_total: usize,
    /// Allocated pages whose checksum verified.
    pub pages_ok: usize,
    /// Pages that are unallocated/free (skipped).
    pub pages_unused: usize,
    /// WAL segment files found.
    pub wal_segments: usize,
    /// WAL records whose CRC verified.
    pub wal_records: usize,
    /// Hard errors (corruption): fail the check.
    pub errors: Vec<String>,
    /// Soft notes (e.g. an incomplete WAL tail after a crash): informational.
    pub warnings: Vec<String>,
}

impl Report {
    /// Whether the database is free of hard errors.
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "database header: {}",
            if self.header_present {
                "present"
            } else {
                "none (page 0 is a data page)"
            }
        )?;
        writeln!(
            f,
            "heap pages: {} total, {} verified, {} unused",
            self.pages_total, self.pages_ok, self.pages_unused
        )?;
        writeln!(
            f,
            "wal: {} segment(s), {} record(s) verified",
            self.wal_segments, self.wal_records
        )?;
        for w in &self.warnings {
            writeln!(f, "  warning: {w}")?;
        }
        for e in &self.errors {
            writeln!(f, "  error:   {e}")?;
        }
        write!(
            f,
            "result: {}",
            if self.is_clean() {
                "clean"
            } else {
                "CORRUPTION DETECTED"
            }
        )
    }
}

/// Check the database directory `dir`, returning a [`Report`].
pub fn check(dir: &Path) -> Report {
    let mut report = Report::default();
    check_heap(&dir.join("heap.db"), &mut report);
    check_wal(&dir.join("wal"), &mut report);
    report
}

fn check_heap(path: &Path, report: &mut Report) {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            report
                .errors
                .push(format!("cannot read {}: {e}", path.display()));
            return;
        }
    };
    if bytes.len() % PAGE_SIZE != 0 {
        report.warnings.push(format!(
            "{} length {} is not a multiple of the page size",
            path.display(),
            bytes.len()
        ));
    }
    report.pages_total = bytes.len() / PAGE_SIZE;

    for index in 0..report.pages_total {
        let page: &[u8; PAGE_SIZE] = bytes[index * PAGE_SIZE..(index + 1) * PAGE_SIZE]
            .try_into()
            .expect("exact page slice");

        // Page 0 may be a database header (a future feature). If it decodes as
        // one, note it; otherwise it is just a data page — fall through and
        // checksum it like any other.
        if index == 0 && DbHeader::decode(page).is_ok() {
            report.header_present = true;
            continue;
        }

        // Unallocated/free pages carry no checksummed contents.
        match PageType::from_u8(page[OFF_PAGE_TYPE]) {
            None | Some(PageType::Free) => report.pages_unused += 1,
            Some(_) => {
                let stored = u16::from_le_bytes([page[OFF_CHECKSUM], page[OFF_CHECKSUM + 1]]);
                let computed = checksum::page_checksum(page);
                if stored == computed {
                    report.pages_ok += 1;
                } else {
                    report.errors.push(format!(
                        "page {index}: checksum mismatch (stored {stored:#06x}, computed {computed:#06x})"
                    ));
                }
            }
        }
    }
}

fn check_wal(dir: &Path, report: &mut Report) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            report
                .errors
                .push(format!("cannot read wal dir {}: {e}", dir.display()));
            return;
        }
    };

    let mut segments: Vec<(u64, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            parse_segment_id(name.to_str()?).map(|id| (id, e.path()))
        })
        .collect();
    segments.sort_by_key(|(id, _)| *id);
    report.wal_segments = segments.len();

    for (id, path) in segments {
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                report
                    .errors
                    .push(format!("cannot read wal segment {id}: {e}"));
                continue;
            }
        };
        if bytes.len() < SEGMENT_HEADER_SIZE || SegmentHeader::decode(&bytes).is_err() {
            report
                .errors
                .push(format!("wal segment {id}: invalid segment header"));
            continue;
        }

        let mut offset = SEGMENT_HEADER_SIZE;
        while offset + RECORD_HEADER_SIZE <= bytes.len() {
            // A zero LSN marks the unwritten (pre-allocated) tail of the segment.
            let lsn = u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("8 bytes"));
            if lsn == 0 {
                break;
            }
            match decode_record(&bytes[offset..]) {
                Ok((_, _, total)) => {
                    report.wal_records += 1;
                    offset += total;
                }
                // A full frame whose CRC is wrong: the bytes are present but
                // corrupt (bit-rot or a torn write) — flag it.
                Err(WalError::CrcMismatch) => {
                    report.errors.push(format!(
                        "wal segment {id}: record at offset {offset} failed CRC (torn write or corruption)"
                    ));
                    break;
                }
                // A frame shorter than declared at the end of the file is the
                // normal incomplete tail of a crash mid-write — a warning.
                Err(WalError::Decode(_)) => {
                    report.warnings.push(format!(
                        "wal segment {id}: incomplete final record at offset {offset} (crash mid-write)"
                    ));
                    break;
                }
                Err(e) => {
                    report
                        .errors
                        .push(format!("wal segment {id}: undecodable record: {e}"));
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use prism_storage::{DiskManager, SlottedPage};
    use prism_testkit::TempDir;
    use prism_wal::record::RecordPayload;
    use prism_wal::{Config as WalConfig, LogRecord, SyncMode, Wal};

    /// Build a small database directory with one valid heap page and one WAL
    /// record, returning its path (and keeping the temp dir alive).
    fn sample_db() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new("fsck").unwrap();
        let dir = tmp.path().to_path_buf();

        // heap.db: page 0 header (written by open) + one valid slotted page.
        {
            let disk = DiskManager::open(&dir.join("heap.db"), true).unwrap();
            let pid = disk.allocate_page().unwrap();
            let mut buf = Box::new([0u8; PAGE_SIZE]);
            {
                let mut page = SlottedPage::init(&mut buf, PageType::Heap);
                page.insert(b"a record").unwrap();
                page.update_checksum();
            }
            disk.write_page(pid, &buf).unwrap();
            disk.sync().unwrap();
            disk.close().unwrap();
        }
        // WAL: one committed record, flushed.
        {
            let wal = Arc::new(
                Wal::open(
                    &dir.join("wal"),
                    WalConfig {
                        segment_size: 64 * 1024,
                        sync_mode: SyncMode::None,
                    },
                )
                .unwrap(),
            );
            // Keep the buffer pool out of it; just append + flush a record.
            let lsn = wal
                .append(LogRecord::txn(
                    2,
                    prism_wal::Lsn::ZERO,
                    RecordPayload::Commit {
                        commit_micros: 0,
                        flags: 0,
                    },
                ))
                .unwrap();
            wal.flush_through(lsn).unwrap();
        }
        (tmp, dir)
    }

    #[test]
    fn clean_database_passes() {
        let (_tmp, dir) = sample_db();
        let report = check(&dir);
        assert!(report.is_clean(), "report not clean: {report}");
        assert_eq!(report.pages_ok, 1, "the one slotted page verified");
        assert_eq!(report.wal_records, 1, "the one WAL record verified");
    }

    #[test]
    fn corrupt_page_is_detected() {
        let (_tmp, dir) = sample_db();
        let heap = dir.join("heap.db");

        // Flip a byte in page 0's body (the allocated Heap page), within the
        // checksummed region [16..].
        let mut bytes = std::fs::read(&heap).unwrap();
        bytes[64] ^= 0xFF;
        std::fs::write(&heap, &bytes).unwrap();

        let report = check(&dir);
        assert!(!report.is_clean(), "corruption should be detected");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("checksum mismatch")),
            "expected a checksum error, got {:?}",
            report.errors
        );
    }

    #[test]
    fn corrupt_wal_record_is_detected() {
        let (_tmp, dir) = sample_db();
        let seg = std::fs::read_dir(dir.join("wal"))
            .unwrap()
            .flatten()
            .find(|e| parse_segment_id(e.file_name().to_str().unwrap()).is_some())
            .unwrap()
            .path();

        // Flip a byte inside the first record's body (after the 32-byte header),
        // which invalidates its CRC.
        let mut bytes = std::fs::read(&seg).unwrap();
        bytes[SEGMENT_HEADER_SIZE + RECORD_HEADER_SIZE + 2] ^= 0xFF;
        std::fs::write(&seg, &bytes).unwrap();

        let report = check(&dir);
        assert!(!report.is_clean(), "WAL corruption should be detected");
        assert!(
            report.errors.iter().any(|e| e.contains("failed CRC")),
            "expected a CRC error, got {:?}",
            report.errors
        );
    }

    #[test]
    fn missing_heap_is_an_error() {
        let tmp = TempDir::new("fsck-empty").unwrap();
        let report = check(tmp.path());
        assert!(!report.is_clean());
    }
}
