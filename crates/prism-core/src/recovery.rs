//! Crash recovery - Analysis + Redo over the WAL.
//!
//! See `docs/components/recovery.md`. **Notable, deliberate deviation:** the
//! classical ARIES *undo* phase (with CLRs) is unnecessary here because the
//! store is MVCC and never overwrites committed data. An aborted or "loser"
//! transaction's effects are neutralized by the visibility function, not by
//! physical undo:
//! - a loser's created versions have `xmin` = the loser → not committed → invisible;
//! - a loser's `xmax` stamps have `xmax` = the loser → "deleted by uncommitted"
//!   → the underlying version stays visible.
//!
//! So recovery is: replay every data record to reconstruct page contents, and
//! rebuild the commit log so visibility classifies losers as not-committed. This
//! is exactly why Postgres has no undo phase. (CLRs / the Abort-undo machinery in
//! the WAL format remain reserved for any future *in-place* structure.)
//!
//! Redo is **incremental** (ARIES-style): for each page it loads the on-disk
//! image and trusts it when the page checksum validates, replaying only records
//! whose LSN is newer than the page's stored `page_lsn`. A page that is missing
//! or torn (checksum mismatch) is rebuilt from scratch (base LSN 0, every record
//! applied), so a possibly-torn page is never trusted. After a [checkpoint]
//! (`RecordStore::checkpoint`, which flushes and fsyncs all dirty pages), the
//! prefix is already on disk and redo skips it - bounding redo work to the tail
//! written since the last checkpoint. The analysis of commit/abort status and
//! the heap directory still scans the full durable log (cheap: no page I/O), so
//! visibility stays exact; WAL truncation is a follow-up that builds on this.
//! The replay iterator stops at the first torn record (CRC mismatch), so exactly
//! the durable prefix is considered.
//!
//! [checkpoint]: crate::store::RecordStore::checkpoint

use std::collections::{BTreeMap, HashMap, HashSet};

use prism_storage::{DiskManager, PAGE_SIZE, PageId, PageType, SlottedPage, checksum};
use prism_wal::record::RecordPayload;
use prism_wal::segment::SEGMENT_HEADER_SIZE;
use prism_wal::{Lsn, Wal};

use crate::error::{CoreError, Result};
use crate::{BOOTSTRAP_TXN, TxnId};

/// The outcome of a recovery pass: what to seed the transaction manager with.
#[derive(Clone, Debug)]
pub struct RecoveryReport {
    /// The next transaction id to allocate (`max observed + 1`).
    pub next_txn_id: TxnId,
    /// WAL records replayed.
    pub records_replayed: usize,
    /// Data records skipped because the on-disk page already reflected them
    /// (its `page_lsn` was at or beyond the record). High after a checkpoint.
    pub records_skipped: usize,
    /// Pages modified and written back (those that needed at least one record
    /// applied on top of their on-disk image).
    pub pages_rebuilt: usize,
    /// Committed transactions and their commit LSNs.
    pub committed: Vec<(TxnId, Lsn)>,
    /// Transactions that aborted or were losers (active at crash) - invisible.
    pub aborted: Vec<TxnId>,
    /// The heap directory: `heap_id -> pages` (allocation order), for seeding
    /// the record store so heaps and `scan` work after restart.
    pub heaps: Vec<(u64, Vec<PageId>)>,
}

/// Recover the database: replay the WAL, rebuild page contents on `disk`, and
/// return the state needed to seed the transaction manager.
///
/// After this returns, reopen the [`DiskManager`] for normal operation so its
/// page allocator accounts for the rebuilt (possibly file-extending) pages.
pub fn recover(wal: &Wal, disk: &DiskManager) -> Result<RecoveryReport> {
    let mut cache = PageCache::new(disk);
    let mut committed: HashMap<TxnId, Lsn> = HashMap::new();
    let mut aborted: HashSet<TxnId> = HashSet::new();
    let mut data_txns: HashSet<TxnId> = HashSet::new();
    // Heap directory rebuilt from HeapPage records, in WAL (allocation) order.
    let mut heap_dir: Vec<(u64, Vec<PageId>)> = Vec::new();
    let mut heap_index: HashMap<u64, usize> = HashMap::new();
    let mut max_txn = BOOTSTRAP_TXN;
    let mut replayed = 0usize;
    let mut skipped = 0usize;

    let from = Lsn::from_parts(0, SEGMENT_HEADER_SIZE as u32);
    for item in wal.replay(from) {
        let (lsn, record) = item?;
        replayed += 1;
        max_txn = max_txn.max(record.txn_id);
        match record.payload {
            RecordPayload::Insert {
                page_id,
                slot_id,
                after_image,
            } => {
                data_txns.insert(record.txn_id);
                cache.load(page_id)?;
                // Already reflected on the on-disk page: nothing to redo.
                if lsn.as_u64() <= cache.base(page_id) {
                    skipped += 1;
                } else {
                    redo_insert(cache.page_mut(page_id), page_id, slot_id, &after_image, lsn)?;
                    cache.mark(page_id);
                }
            }
            RecordPayload::Update {
                page_id,
                slot_id,
                after_image,
                ..
            } => {
                data_txns.insert(record.txn_id);
                cache.load(page_id)?;
                if lsn.as_u64() <= cache.base(page_id) {
                    skipped += 1;
                } else {
                    redo_overwrite(cache.page_mut(page_id), page_id, slot_id, &after_image, lsn)?;
                    cache.mark(page_id);
                }
            }
            RecordPayload::Delete {
                page_id,
                slot_id,
                before_image,
            } => {
                data_txns.insert(record.txn_id);
                cache.load(page_id)?;
                if lsn.as_u64() <= cache.base(page_id) {
                    skipped += 1;
                } else {
                    redo_delete(
                        cache.page_mut(page_id),
                        page_id,
                        slot_id,
                        &before_image,
                        record.txn_id,
                        lsn,
                    )?;
                    cache.mark(page_id);
                }
            }
            RecordPayload::FullPageImage { page_id, image } => {
                data_txns.insert(record.txn_id);
                if image.len() != PAGE_SIZE {
                    return Err(CoreError::Recovery(format!(
                        "full-page image for page {} is {} bytes",
                        page_id.as_u64(),
                        image.len()
                    )));
                }
                cache.load(page_id)?;
                if lsn.as_u64() <= cache.base(page_id) {
                    skipped += 1;
                } else {
                    let buf = cache.page_mut(page_id);
                    buf.copy_from_slice(&image);
                    cache.mark(page_id);
                }
            }
            RecordPayload::HeapPage { heap_id, page_id } => {
                let idx = *heap_index.entry(heap_id).or_insert_with(|| {
                    heap_dir.push((heap_id, Vec::new()));
                    heap_dir.len() - 1
                });
                heap_dir[idx].1.push(page_id);
            }
            RecordPayload::Commit { .. } => {
                committed.insert(record.txn_id, lsn);
            }
            RecordPayload::Abort => {
                aborted.insert(record.txn_id);
            }
            // No physical undo under MVCC; checkpoint markers carry no redo.
            RecordPayload::Clr { .. }
            | RecordPayload::BeginCheckpoint { .. }
            | RecordPayload::CheckpointContents { .. }
            | RecordPayload::EndCheckpoint { .. } => {}
        }
    }

    // Write back only the pages we actually changed (with refreshed checksums)
    // and make them durable. Pages already current on disk are left untouched.
    let pages_rebuilt = cache.flush_dirty()?;
    disk.sync()?;

    // Losers (data records but neither committed nor explicitly aborted) are
    // treated as aborted - their effects stay invisible.
    for &t in &data_txns {
        if !committed.contains_key(&t) && !aborted.contains(&t) {
            aborted.insert(t);
        }
    }

    Ok(RecoveryReport {
        next_txn_id: max_txn + 1,
        records_replayed: replayed,
        records_skipped: skipped,
        pages_rebuilt,
        committed: committed.into_iter().collect(),
        aborted: aborted.into_iter().collect(),
        heaps: heap_dir,
    })
}

/// In-memory pages being recovered: each is loaded once from disk (trusted if
/// its checksum validates, giving a base LSN), modified by redo, and written
/// back only if it changed.
struct PageCache<'a> {
    disk: &'a DiskManager,
    pages: BTreeMap<u64, Box<[u8; PAGE_SIZE]>>,
    base: HashMap<u64, u64>,
    dirtied: HashSet<u64>,
}

impl<'a> PageCache<'a> {
    fn new(disk: &'a DiskManager) -> Self {
        Self {
            disk,
            pages: BTreeMap::new(),
            base: HashMap::new(),
            dirtied: HashSet::new(),
        }
    }

    /// Load `page_id` on first use. A valid on-disk page is trusted (its
    /// `page_lsn` becomes the base below which records are already applied); a
    /// missing or torn page is rebuilt from scratch (base 0, empty heap page).
    fn load(&mut self, page_id: PageId) -> Result<&mut Box<[u8; PAGE_SIZE]>> {
        let key = page_id.as_u64();
        if let std::collections::btree_map::Entry::Vacant(slot) = self.pages.entry(key) {
            let mut buf = Box::new([0u8; PAGE_SIZE]);
            let read_ok = self.disk.read_page(page_id, &mut buf).is_ok();
            let stored = u16::from_le_bytes([buf[8], buf[9]]);
            let valid = read_ok && stored == checksum::page_checksum(&buf);
            let base = if valid {
                u64::from_le_bytes(buf[0..8].try_into().expect("8 bytes"))
            } else {
                // Torn or never-written: start empty and replay every record.
                SlottedPage::init(&mut buf, PageType::Heap);
                0
            };
            self.base.insert(key, base);
            slot.insert(buf);
        }
        Ok(self.pages.get_mut(&key).expect("page loaded"))
    }

    fn base(&self, page_id: PageId) -> u64 {
        self.base[&page_id.as_u64()]
    }

    fn page_mut(&mut self, page_id: PageId) -> &mut Box<[u8; PAGE_SIZE]> {
        self.pages.get_mut(&page_id.as_u64()).expect("page loaded")
    }

    fn mark(&mut self, page_id: PageId) {
        self.dirtied.insert(page_id.as_u64());
    }

    /// Refresh checksums on and persist every modified page; returns the count.
    fn flush_dirty(&mut self) -> Result<usize> {
        for &key in &self.dirtied {
            let buf = self.pages.get_mut(&key).expect("dirtied page loaded");
            SlottedPage::new(buf).update_checksum();
            self.disk.write_page(PageId(key), buf)?;
        }
        Ok(self.dirtied.len())
    }
}

fn redo_insert(
    buf: &mut [u8; PAGE_SIZE],
    page_id: PageId,
    slot_id: u16,
    after_image: &[u8],
    lsn: Lsn,
) -> Result<()> {
    let mut page = SlottedPage::new(buf);
    match page.insert(after_image) {
        Some(slot) if slot == slot_id => {
            page.set_page_lsn(lsn.as_u64());
            Ok(())
        }
        Some(other) => Err(CoreError::Recovery(format!(
            "redo insert on page {}: expected slot {slot_id}, got {other}",
            page_id.as_u64()
        ))),
        None => Err(CoreError::Recovery(format!(
            "redo insert on page {}: record did not fit",
            page_id.as_u64()
        ))),
    }
}

fn redo_overwrite(
    buf: &mut [u8; PAGE_SIZE],
    page_id: PageId,
    slot_id: u16,
    after_image: &[u8],
    lsn: Lsn,
) -> Result<()> {
    let mut page = SlottedPage::new(buf);
    let dst = page.get_mut(slot_id).ok_or_else(|| {
        CoreError::Recovery(format!(
            "redo update on missing slot {slot_id} of page {}",
            page_id.as_u64()
        ))
    })?;
    if dst.len() != after_image.len() {
        return Err(CoreError::Recovery("redo update length mismatch".into()));
    }
    dst.copy_from_slice(after_image);
    page.set_page_lsn(lsn.as_u64());
    Ok(())
}

fn redo_delete(
    buf: &mut [u8; PAGE_SIZE],
    page_id: PageId,
    slot_id: u16,
    before_image: &[u8],
    txn_id: TxnId,
    lsn: Lsn,
) -> Result<()> {
    let mut page = SlottedPage::new(buf);
    let dst = page.get_mut(slot_id).ok_or_else(|| {
        CoreError::Recovery(format!(
            "redo delete on missing slot {slot_id} of page {}",
            page_id.as_u64()
        ))
    })?;
    if dst.len() != before_image.len() || dst.len() < 16 {
        return Err(CoreError::Recovery("redo delete length mismatch".into()));
    }
    dst.copy_from_slice(before_image);
    dst[8..16].copy_from_slice(&txn_id.to_le_bytes()); // stamp xmax = deleting txn
    page.set_page_lsn(lsn.as_u64());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::RecordId;
    use crate::store::{HeapId, RecordStore};
    use crate::txn::{TxnManager, TxnMode};
    use prism_buffer::{BufferPool, Config as BufConfig};
    use prism_testkit::TempDir;
    use prism_wal::{Config as WalConfig, SyncMode};
    use std::sync::Arc;

    const HEAP: HeapId = HeapId(1);

    fn open_wal(dir: &std::path::Path) -> Arc<Wal> {
        Arc::new(
            Wal::open(
                &dir.join("wal"),
                WalConfig {
                    segment_size: 256 * 1024,
                    sync_mode: SyncMode::None,
                },
            )
            .unwrap(),
        )
    }

    #[test]
    fn recovers_committed_data_and_hides_losers() {
        let tmp = TempDir::new("recover").unwrap();
        let heap = tmp.path().join("heap.db");

        // RIDs captured before the crash.
        let (rid_alpha, rid_beta, rid_beta2, rid_ghost);

        // ── Workload, then a simulated crash (drop everything, no clean flush). ──
        {
            let disk = Arc::new(DiskManager::open(&heap, true).unwrap());
            let wal = open_wal(tmp.path());
            let buffer = Arc::new(
                BufferPool::new(disk.clone(), wal.clone(), BufConfig { frame_count: 8 }).unwrap(),
            );
            let txns = Arc::new(TxnManager::new(wal.clone()));
            let store = RecordStore::new(buffer.clone(), wal.clone(), txns.clone());

            let t1 = txns.begin(TxnMode::ReadWrite); // id 2
            rid_alpha = store.insert(&t1, HEAP, b"alpha").unwrap();
            rid_beta = store.insert(&t1, HEAP, b"beta").unwrap();
            t1.commit().unwrap();

            let t2 = txns.begin(TxnMode::ReadWrite); // id 3
            rid_beta2 = store.update(&t2, rid_beta, b"beta-updated").unwrap();
            t2.commit().unwrap();

            let t3 = txns.begin(TxnMode::ReadWrite); // id 4 - the loser
            rid_ghost = store.insert(&t3, HEAP, b"ghost").unwrap();
            std::mem::forget(t3); // crash before commit/abort: no finalize record

            // Crash: drop in-memory state WITHOUT flushing the buffer pool.
            drop(store);
            drop(buffer);
            drop(txns);
            drop(disk);
            // wal Arc dropped at scope end (files persist).
        }

        // ── Recovery. ──
        let wal = open_wal(tmp.path());
        let report = {
            let disk = DiskManager::open(&heap, false).unwrap();
            let r = recover(&wal, &disk).unwrap();
            disk.close().unwrap();
            r
        };
        assert_eq!(report.next_txn_id, 5, "max txn id was 4 (the loser)");
        assert!(report.pages_rebuilt >= 1);

        // ── Reopen for normal operation, seeded from the recovery report. ──
        let disk = Arc::new(DiskManager::open(&heap, false).unwrap());
        let buffer = Arc::new(
            BufferPool::new(disk.clone(), wal.clone(), BufConfig { frame_count: 8 }).unwrap(),
        );
        let txns = Arc::new(TxnManager::new_recovered(
            wal.clone(),
            report.next_txn_id,
            &report.committed,
            &report.aborted,
        ));
        let store = RecordStore::new(buffer, wal.clone(), txns.clone());

        let reader = txns.begin(TxnMode::ReadOnly);
        let read = |rid: RecordId| store.read(&reader, rid).unwrap();

        assert_eq!(
            read(rid_alpha).as_deref(),
            Some(&b"alpha"[..]),
            "committed insert survived"
        );
        assert_eq!(
            read(rid_beta2).as_deref(),
            Some(&b"beta-updated"[..]),
            "committed update survived"
        );
        assert_eq!(
            read(rid_beta),
            None,
            "old version superseded by committed update"
        );
        assert_eq!(
            read(rid_ghost),
            None,
            "loser's insert is invisible after recovery"
        );
        reader.commit().unwrap();
    }

    #[test]
    fn checkpoint_makes_recovery_skip_the_flushed_prefix() {
        let tmp = TempDir::new("recover-ckpt").unwrap();
        let heap = tmp.path().join("heap.db");

        {
            let disk = Arc::new(DiskManager::open(&heap, true).unwrap());
            let wal = open_wal(tmp.path());
            let buffer = Arc::new(
                BufferPool::new(disk.clone(), wal.clone(), BufConfig { frame_count: 16 }).unwrap(),
            );
            let txns = Arc::new(TxnManager::new(wal.clone()));
            let store = RecordStore::new(buffer.clone(), wal.clone(), txns.clone());

            // First batch, committed and then checkpointed (flushed to disk).
            let t1 = txns.begin(TxnMode::ReadWrite);
            for i in 0..30u8 {
                store.insert(&t1, HEAP, &[i; 24]).unwrap();
            }
            t1.commit().unwrap();
            store.checkpoint().unwrap(); // <-- flush all pages to disk

            // Second batch, committed but NOT checkpointed (still only in the WAL
            // and the dirty buffer pool).
            let t2 = txns.begin(TxnMode::ReadWrite);
            for i in 30..60u8 {
                store.insert(&t2, HEAP, &[i; 24]).unwrap();
            }
            t2.commit().unwrap();

            // Crash: drop in-memory state without flushing the second batch.
            drop(store);
            drop(buffer);
            drop(txns);
            drop(disk);
        }

        // Recovery should skip the checkpointed first batch and only replay the
        // second batch's records.
        let wal = open_wal(tmp.path());
        let report = {
            let disk = DiskManager::open(&heap, false).unwrap();
            let r = recover(&wal, &disk).unwrap();
            disk.close().unwrap();
            r
        };
        assert!(
            report.records_skipped >= 30,
            "checkpointed prefix should be skipped, skipped only {}",
            report.records_skipped
        );

        // All 60 rows are present and visible after recovery.
        let disk = Arc::new(DiskManager::open(&heap, false).unwrap());
        let buffer =
            Arc::new(BufferPool::new(disk, wal.clone(), BufConfig { frame_count: 16 }).unwrap());
        let txns = Arc::new(TxnManager::new_recovered(
            wal.clone(),
            report.next_txn_id,
            &report.committed,
            &report.aborted,
        ));
        let store = RecordStore::new(buffer, wal, txns.clone());
        store.seed_heap_directory(&report.heaps);

        let reader = txns.begin(TxnMode::ReadOnly);
        let rows = store.scan(&reader, HEAP).unwrap();
        assert_eq!(
            rows.len(),
            60,
            "both batches survive (checkpointed + replayed)"
        );
        reader.commit().unwrap();
    }

    #[test]
    fn scan_works_after_recovery() {
        let tmp = TempDir::new("recover-scan").unwrap();
        let heap = tmp.path().join("heap.db");

        {
            let disk = Arc::new(DiskManager::open(&heap, true).unwrap());
            let wal = open_wal(tmp.path());
            let buffer = Arc::new(
                BufferPool::new(disk.clone(), wal.clone(), BufConfig { frame_count: 4 }).unwrap(),
            );
            let txns = Arc::new(TxnManager::new(wal.clone()));
            let store = RecordStore::new(buffer.clone(), wal.clone(), txns.clone());

            let t = txns.begin(TxnMode::ReadWrite);
            for i in 0..20u8 {
                store.insert(&t, HEAP, &[i; 32]).unwrap();
            }
            t.commit().unwrap();

            drop(store);
            drop(buffer);
            drop(txns);
            drop(disk);
        }

        let wal = open_wal(tmp.path());
        let report = {
            let disk = DiskManager::open(&heap, false).unwrap();
            let r = recover(&wal, &disk).unwrap();
            disk.close().unwrap();
            r
        };
        assert!(!report.heaps.is_empty(), "heap directory was rebuilt");

        let disk = Arc::new(DiskManager::open(&heap, false).unwrap());
        let buffer =
            Arc::new(BufferPool::new(disk, wal.clone(), BufConfig { frame_count: 4 }).unwrap());
        let txns = Arc::new(TxnManager::new_recovered(
            wal.clone(),
            report.next_txn_id,
            &report.committed,
            &report.aborted,
        ));
        let store = RecordStore::new(buffer, wal, txns.clone());
        store.seed_heap_directory(&report.heaps); // <-- the directory makes scan work

        let reader = txns.begin(TxnMode::ReadOnly);
        let rows = store.scan(&reader, HEAP).unwrap();
        assert_eq!(
            rows.len(),
            20,
            "all committed rows visible via scan after recovery"
        );
        reader.commit().unwrap();
    }
}
