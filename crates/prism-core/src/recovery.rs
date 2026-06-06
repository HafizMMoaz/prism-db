//! Crash recovery — Analysis + Redo over the WAL.
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
//! Scope (this increment): no checkpoints yet, so redo replays the entire durable
//! WAL prefix and rebuilds every touched page from scratch — never trusting a
//! possibly-torn on-disk page. Checkpoints + page-LSN-skipping incremental redo
//! are a performance follow-up. The WAL replay iterator stops at the first torn
//! record (CRC mismatch), so exactly the durable prefix is applied.

use std::collections::{BTreeMap, HashMap, HashSet};

use prism_storage::{DiskManager, PAGE_SIZE, PageId, PageType, SlottedPage};
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
    /// Pages rebuilt and written back.
    pub pages_rebuilt: usize,
    /// Committed transactions and their commit LSNs.
    pub committed: Vec<(TxnId, Lsn)>,
    /// Transactions that aborted or were losers (active at crash) — invisible.
    pub aborted: Vec<TxnId>,
}

/// Recover the database: replay the WAL, rebuild page contents on `disk`, and
/// return the state needed to seed the transaction manager.
///
/// After this returns, reopen the [`DiskManager`] for normal operation so its
/// page allocator accounts for the rebuilt (possibly file-extending) pages.
pub fn recover(wal: &Wal, disk: &DiskManager) -> Result<RecoveryReport> {
    let mut pages: BTreeMap<u64, Box<[u8; PAGE_SIZE]>> = BTreeMap::new();
    let mut committed: HashMap<TxnId, Lsn> = HashMap::new();
    let mut aborted: HashSet<TxnId> = HashSet::new();
    let mut data_txns: HashSet<TxnId> = HashSet::new();
    let mut max_txn = BOOTSTRAP_TXN;
    let mut replayed = 0usize;

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
                redo_insert(&mut pages, page_id, slot_id, &after_image, lsn)?;
                data_txns.insert(record.txn_id);
            }
            RecordPayload::Update {
                page_id,
                slot_id,
                after_image,
                ..
            } => {
                redo_overwrite(&mut pages, page_id, slot_id, &after_image, lsn)?;
                data_txns.insert(record.txn_id);
            }
            RecordPayload::Delete {
                page_id,
                slot_id,
                before_image,
            } => {
                redo_delete(
                    &mut pages,
                    page_id,
                    slot_id,
                    &before_image,
                    record.txn_id,
                    lsn,
                )?;
                data_txns.insert(record.txn_id);
            }
            RecordPayload::FullPageImage { page_id, image } => {
                let mut buf = Box::new([0u8; PAGE_SIZE]);
                if image.len() != PAGE_SIZE {
                    return Err(CoreError::Recovery(format!(
                        "full-page image for page {} is {} bytes",
                        page_id.as_u64(),
                        image.len()
                    )));
                }
                buf.copy_from_slice(&image);
                pages.insert(page_id.as_u64(), buf);
                data_txns.insert(record.txn_id);
            }
            RecordPayload::Commit { .. } => {
                committed.insert(record.txn_id, lsn);
            }
            RecordPayload::Abort => {
                aborted.insert(record.txn_id);
            }
            // No physical undo under MVCC; checkpoint markers are not used yet.
            RecordPayload::Clr { .. }
            | RecordPayload::BeginCheckpoint { .. }
            | RecordPayload::CheckpointContents { .. }
            | RecordPayload::EndCheckpoint { .. } => {}
        }
    }

    // Write rebuilt pages back (with refreshed checksums) and make them durable.
    let pages_rebuilt = pages.len();
    for (pidv, mut buf) in pages {
        SlottedPage::new(&mut buf).update_checksum();
        disk.write_page(PageId(pidv), &buf)?;
    }
    disk.sync()?;

    // Losers (data records but neither committed nor explicitly aborted) are
    // treated as aborted — their effects stay invisible.
    for &t in &data_txns {
        if !committed.contains_key(&t) && !aborted.contains(&t) {
            aborted.insert(t);
        }
    }

    Ok(RecoveryReport {
        next_txn_id: max_txn + 1,
        records_replayed: replayed,
        pages_rebuilt,
        committed: committed.into_iter().collect(),
        aborted: aborted.into_iter().collect(),
    })
}

fn redo_insert(
    pages: &mut BTreeMap<u64, Box<[u8; PAGE_SIZE]>>,
    page_id: PageId,
    slot_id: u16,
    after_image: &[u8],
    lsn: Lsn,
) -> Result<()> {
    let buf = pages.entry(page_id.as_u64()).or_insert_with(|| {
        let mut b = Box::new([0u8; PAGE_SIZE]);
        SlottedPage::init(&mut b, PageType::Heap);
        b
    });
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
    pages: &mut BTreeMap<u64, Box<[u8; PAGE_SIZE]>>,
    page_id: PageId,
    slot_id: u16,
    after_image: &[u8],
    lsn: Lsn,
) -> Result<()> {
    let buf = pages.get_mut(&page_id.as_u64()).ok_or_else(|| {
        CoreError::Recovery(format!("redo update on missing page {}", page_id.as_u64()))
    })?;
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
    pages: &mut BTreeMap<u64, Box<[u8; PAGE_SIZE]>>,
    page_id: PageId,
    slot_id: u16,
    before_image: &[u8],
    txn_id: TxnId,
    lsn: Lsn,
) -> Result<()> {
    let buf = pages.get_mut(&page_id.as_u64()).ok_or_else(|| {
        CoreError::Recovery(format!("redo delete on missing page {}", page_id.as_u64()))
    })?;
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

            let t3 = txns.begin(TxnMode::ReadWrite); // id 4 — the loser
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
}
