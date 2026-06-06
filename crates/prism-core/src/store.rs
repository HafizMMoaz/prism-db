//! The transactional record store — MVCC tuple operations over the buffer pool.
//!
//! This is the seam all three access methods share: it writes [`RecordHeader`]s
//! and payloads into slotted heap pages through the buffer pool, logs every
//! mutation to the WAL, and resolves reads through the snapshot-isolation
//! visibility function, walking the `prev_version` chain. See
//! `docs/components/mvcc.md`.
//!
//! Scope (this increment): insert/read/update/delete/scan with version chains
//! and first-committer-wins conflict detection. Blocking write locks (the lock
//! manager) and undo/redo recovery are later increments. Heap free-space uses a
//! simple "last page, else new page" probe under one lock; a free-space map is a
//! documented follow-up.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use prism_buffer::{BufferPool, PageWriteGuard};
use prism_storage::{PAGE_SIZE, PageId, PageType, SlottedPage, SlottedPageRef, checksum};
use prism_wal::record::RecordPayload;
use prism_wal::{LogRecord, Lsn, Wal};

use crate::TxnManager;
use crate::error::{CoreError, Result};
use crate::record::{RECORD_HEADER_SIZE, RecordHeader, RecordId};
use crate::txn::TxnHandle;
use crate::visibility::visible;

/// Identifies a heap (a table, collection, or namespace) by its object id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct HeapId(pub u64);

/// Maps heaps to their pages, and pages back to their heaps.
#[derive(Default)]
struct HeapTables {
    pages: HashMap<HeapId, Vec<PageId>>,
    page_heap: HashMap<PageId, HeapId>,
}

/// The transactional record store.
pub struct RecordStore {
    buffer: Arc<BufferPool>,
    wal: Arc<Wal>,
    txns: Arc<TxnManager>,
    heaps: Mutex<HeapTables>,
}

impl RecordStore {
    /// Create a record store over the given buffer pool, WAL, and txn manager.
    pub fn new(buffer: Arc<BufferPool>, wal: Arc<Wal>, txns: Arc<TxnManager>) -> Self {
        Self {
            buffer,
            wal,
            txns,
            heaps: Mutex::new(HeapTables::default()),
        }
    }

    /// Insert a new record into `heap`, returning its id.
    pub fn insert(&self, txn: &TxnHandle, heap: HeapId, payload: &[u8]) -> Result<RecordId> {
        let header = RecordHeader::new_insert(txn.id());
        let bytes = encode_record(&header, payload);
        self.insert_bytes(txn, heap, &bytes)
    }

    /// Read the version of `rid` visible to `txn`, walking the version chain back
    /// in time. Returns the visible payload, or `None` if no version is visible.
    pub fn read(&self, txn: &TxnHandle, rid: RecordId) -> Result<Option<Vec<u8>>> {
        let mut cursor = Some(rid);
        while let Some(r) = cursor {
            let guard = self.buffer.fetch_read(r.page)?;
            let page = SlottedPageRef::new(&guard);
            let Some(rec) = page.get(r.slot) else {
                return Ok(None); // slot gone — end of chain
            };
            let Some(header) = RecordHeader::decode(rec) else {
                return Ok(None);
            };
            if visible(&header, txn.snapshot(), self.txns.commit_log()) {
                return Ok(Some(rec[RECORD_HEADER_SIZE..].to_vec()));
            }
            cursor = header.prev_version;
        }
        Ok(None)
    }

    /// Update the record at `rid` (which must be the newest version visible to
    /// `txn`), returning the new version's id.
    ///
    /// Fails with [`CoreError::SerializationFailure`] if another transaction has
    /// already superseded this version (first-committer-wins).
    pub fn update(&self, txn: &TxnHandle, rid: RecordId, payload: &[u8]) -> Result<RecordId> {
        let heap = self.heap_of(rid.page)?;

        // Take the write lock first (blocks behind an in-progress writer; held
        // until this txn commits/aborts), then validate the post-wait state.
        let locks = self.txns.locks();
        locks.acquire(txn.id(), rid, locks.default_timeout())?;

        // Validate the target version before producing a new one.
        {
            let guard = self.buffer.fetch_read(rid.page)?;
            let page = SlottedPageRef::new(&guard);
            let header = page
                .get(rid.slot)
                .and_then(RecordHeader::decode)
                .ok_or(CoreError::SerializationFailure)?;
            self.check_writable(&header, txn)?;
        }

        // Place the new version, chained to the old one.
        let new_header = RecordHeader {
            xmin: txn.id(),
            xmax: crate::NO_TXN,
            prev_version: Some(rid),
            flags: 0,
        };
        let new_bytes = encode_record(&new_header, payload);
        let new_rid = self.insert_bytes(txn, heap, &new_bytes)?;

        // Stamp the old version's xmax = our txn id.
        self.stamp_xmax(txn, rid, /* is_delete */ false)?;
        Ok(new_rid)
    }

    /// Mark the record at `rid` deleted (set its `xmax`).
    pub fn delete(&self, txn: &TxnHandle, rid: RecordId) -> Result<()> {
        let locks = self.txns.locks();
        locks.acquire(txn.id(), rid, locks.default_timeout())?;
        {
            let guard = self.buffer.fetch_read(rid.page)?;
            let page = SlottedPageRef::new(&guard);
            let header = page
                .get(rid.slot)
                .and_then(RecordHeader::decode)
                .ok_or(CoreError::SerializationFailure)?;
            self.check_writable(&header, txn)?;
        }
        self.stamp_xmax(txn, rid, /* is_delete */ true)
    }

    /// All records in `heap` visible to `txn`, as `(rid, payload)` pairs.
    pub fn scan(&self, txn: &TxnHandle, heap: HeapId) -> Result<Vec<(RecordId, Vec<u8>)>> {
        let pages = {
            let tables = self.heaps.lock().expect("heaps poisoned");
            tables.pages.get(&heap).cloned().unwrap_or_default()
        };
        let mut out = Vec::new();
        for pid in pages {
            let guard = self.buffer.fetch_read(pid)?;
            let page = SlottedPageRef::new(&guard);
            for slot in 0..page.slot_count() {
                let Some(rec) = page.get(slot) else { continue };
                let Some(header) = RecordHeader::decode(rec) else {
                    continue;
                };
                if visible(&header, txn.snapshot(), self.txns.commit_log()) {
                    out.push((RecordId::new(pid, slot), rec[RECORD_HEADER_SIZE..].to_vec()));
                }
            }
        }
        Ok(out)
    }

    // ── Internals ───────────────────────────────────────────────────────

    /// First-committer-wins check: the version must be visible to us and not yet
    /// superseded by anyone. (Blocking on in-progress writers is the lock
    /// manager's job; here we conservatively fail-fast.)
    fn check_writable(&self, header: &RecordHeader, txn: &TxnHandle) -> Result<()> {
        if !visible(header, txn.snapshot(), self.txns.commit_log()) {
            return Err(CoreError::SerializationFailure);
        }
        if header.is_deleted() && header.xmax != txn.id() {
            return Err(CoreError::SerializationFailure);
        }
        Ok(())
    }

    fn heap_of(&self, page: PageId) -> Result<HeapId> {
        self.heaps
            .lock()
            .expect("heaps poisoned")
            .page_heap
            .get(&page)
            .copied()
            .ok_or(CoreError::SerializationFailure)
    }

    /// Place raw record bytes into `heap`: try existing pages, else a new page.
    fn insert_bytes(&self, txn: &TxnHandle, heap: HeapId, bytes: &[u8]) -> Result<RecordId> {
        let mut tables = self.heaps.lock().expect("heaps poisoned");
        let candidates: Vec<PageId> = tables.pages.get(&heap).cloned().unwrap_or_default();

        // Newest pages first — most likely to have room.
        for &pid in candidates.iter().rev() {
            let mut guard = self.buffer.fetch_write(pid)?;
            let slot = {
                let mut page = SlottedPage::new(&mut guard);
                page.insert(bytes)
            };
            if let Some(slot) = slot {
                self.log_insert(txn, &mut guard, pid, slot, bytes)?;
                return Ok(RecordId::new(pid, slot));
            }
        }

        // No room: allocate a fresh heap page.
        let mut guard = self.buffer.new_page()?;
        let pid = guard.page_id();
        let slot = {
            let mut page = SlottedPage::init(&mut guard, PageType::Heap);
            page.insert(bytes).ok_or(CoreError::SerializationFailure)? // record too large for an empty page
        };
        self.log_insert(txn, &mut guard, pid, slot, bytes)?;
        tables.pages.entry(heap).or_default().push(pid);
        tables.page_heap.insert(pid, heap);
        Ok(RecordId::new(pid, slot))
    }

    fn log_insert(
        &self,
        txn: &TxnHandle,
        guard: &mut PageWriteGuard<'_>,
        page: PageId,
        slot: u16,
        bytes: &[u8],
    ) -> Result<()> {
        let lsn = self.log(
            txn,
            RecordPayload::Insert {
                page_id: page,
                slot_id: slot,
                after_image: bytes.to_vec(),
            },
        )?;
        finalize_page(guard, lsn);
        Ok(())
    }

    /// Stamp `rid`'s header with `xmax = txn.id()` in place, logging it.
    fn stamp_xmax(&self, txn: &TxnHandle, rid: RecordId, is_delete: bool) -> Result<()> {
        let mut guard = self.buffer.fetch_write(rid.page)?;
        let (before, after) = {
            let mut page = SlottedPage::new(&mut guard);
            let before = page
                .get(rid.slot)
                .ok_or(CoreError::SerializationFailure)?
                .to_vec();
            let mut after = before.clone();
            after[8..16].copy_from_slice(&txn.id().to_le_bytes()); // xmax field
            page.get_mut(rid.slot)
                .ok_or(CoreError::SerializationFailure)?
                .copy_from_slice(&after);
            (before, after)
        };
        let payload = if is_delete {
            RecordPayload::Delete {
                page_id: rid.page,
                slot_id: rid.slot,
                before_image: before,
            }
        } else {
            RecordPayload::Update {
                page_id: rid.page,
                slot_id: rid.slot,
                before_image: before,
                after_image: after,
            }
        };
        let lsn = self.log(txn, payload)?;
        finalize_page(&mut guard, lsn);
        Ok(())
    }

    /// Append a WAL record for `txn`, advancing its `prev_lsn` chain.
    fn log(&self, txn: &TxnHandle, payload: RecordPayload) -> Result<Lsn> {
        let lsn = self
            .wal
            .append(LogRecord::txn(txn.id(), txn.last_lsn(), payload))?;
        txn.set_last_lsn(lsn);
        Ok(lsn)
    }
}

/// Encode a record: 24-byte MVCC header followed by the payload.
fn encode_record(header: &RecordHeader, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(RECORD_HEADER_SIZE + payload.len());
    bytes.extend_from_slice(&header.encode());
    bytes.extend_from_slice(payload);
    bytes
}

/// Stamp the page LSN and refresh the page checksum before the guard drops.
fn finalize_page(guard: &mut PageWriteGuard<'_>, lsn: Lsn) {
    guard.set_page_lsn(lsn);
    let bytes: &[u8; PAGE_SIZE] = guard;
    let crc = checksum::page_checksum(bytes);
    guard[8..10].copy_from_slice(&crc.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::txn::TxnMode;
    use prism_buffer::Config as BufConfig;
    use prism_storage::DiskManager;
    use prism_testkit::TempDir;
    use prism_wal::{Config as WalConfig, SyncMode};

    const HEAP: HeapId = HeapId(1);

    struct Env {
        store: RecordStore,
        txns: Arc<TxnManager>,
        _buffer: Arc<BufferPool>,
        _wal: Arc<Wal>,
        _disk: Arc<DiskManager>,
        _tmp: TempDir,
    }

    impl Env {
        fn new(frames: usize) -> Self {
            let tmp = TempDir::new("store").unwrap();
            let disk = Arc::new(DiskManager::open(&tmp.path().join("heap.db"), true).unwrap());
            let wal = Arc::new(
                Wal::open(
                    &tmp.path().join("wal"),
                    WalConfig {
                        segment_size: 256 * 1024,
                        sync_mode: SyncMode::None,
                    },
                )
                .unwrap(),
            );
            let buffer = Arc::new(
                BufferPool::new(
                    disk.clone(),
                    wal.clone(),
                    BufConfig {
                        frame_count: frames,
                    },
                )
                .unwrap(),
            );
            let txns = Arc::new(TxnManager::new(wal.clone()));
            let store = RecordStore::new(buffer.clone(), wal.clone(), txns.clone());
            Env {
                store,
                txns,
                _buffer: buffer,
                _wal: wal,
                _disk: disk,
                _tmp: tmp,
            }
        }
    }

    #[test]
    fn insert_then_read_own_write() {
        let env = Env::new(16);
        let txn = env.txns.begin(TxnMode::ReadWrite);
        let rid = env.store.insert(&txn, HEAP, b"hello").unwrap();
        assert_eq!(
            env.store.read(&txn, rid).unwrap().as_deref(),
            Some(&b"hello"[..])
        );
        txn.commit().unwrap();
    }

    #[test]
    fn committed_insert_visible_to_later_txn_not_earlier() {
        let env = Env::new(16);

        // An earlier reader begins before the writer commits.
        let early = env.txns.begin(TxnMode::ReadWrite);

        let writer = env.txns.begin(TxnMode::ReadWrite);
        let rid = env.store.insert(&writer, HEAP, b"v1").unwrap();
        writer.commit().unwrap();

        // The early reader's snapshot predates the commit → invisible.
        assert_eq!(env.store.read(&early, rid).unwrap(), None);
        early.commit().unwrap();

        // A reader that begins after the commit sees it.
        let late = env.txns.begin(TxnMode::ReadOnly);
        assert_eq!(
            env.store.read(&late, rid).unwrap().as_deref(),
            Some(&b"v1"[..])
        );
        late.commit().unwrap();
    }

    #[test]
    fn uncommitted_insert_invisible_to_others() {
        let env = Env::new(16);
        let writer = env.txns.begin(TxnMode::ReadWrite);
        let rid = env.store.insert(&writer, HEAP, b"pending").unwrap();

        let other = env.txns.begin(TxnMode::ReadOnly);
        assert_eq!(env.store.read(&other, rid).unwrap(), None);
        other.commit().unwrap();
        writer.abort().unwrap();
    }

    #[test]
    fn update_creates_visible_new_version() {
        let env = Env::new(16);
        let t1 = env.txns.begin(TxnMode::ReadWrite);
        let rid = env.store.insert(&t1, HEAP, b"v1").unwrap();
        t1.commit().unwrap();

        let t2 = env.txns.begin(TxnMode::ReadWrite);
        let new_rid = env.store.update(&t2, rid, b"v2-longer").unwrap();
        // Within t2, reading the new rid sees the new value.
        assert_eq!(
            env.store.read(&t2, new_rid).unwrap().as_deref(),
            Some(&b"v2-longer"[..])
        );
        t2.commit().unwrap();

        let t3 = env.txns.begin(TxnMode::ReadOnly);
        assert_eq!(
            env.store.read(&t3, new_rid).unwrap().as_deref(),
            Some(&b"v2-longer"[..])
        );
        t3.commit().unwrap();
    }

    #[test]
    fn old_snapshot_walks_chain_to_old_version() {
        let env = Env::new(16);
        let t1 = env.txns.begin(TxnMode::ReadWrite);
        let rid = env.store.insert(&t1, HEAP, b"v1").unwrap();
        t1.commit().unwrap();

        // Reader begins now — sees v1.
        let reader = env.txns.begin(TxnMode::ReadOnly);

        // A later writer updates the row and commits.
        let t2 = env.txns.begin(TxnMode::ReadWrite);
        let new_rid = env.store.update(&t2, rid, b"v2").unwrap();
        t2.commit().unwrap();

        // Given the newest rid, the old reader walks back to the visible v1.
        assert_eq!(
            env.store.read(&reader, new_rid).unwrap().as_deref(),
            Some(&b"v1"[..])
        );
        reader.commit().unwrap();
    }

    #[test]
    fn delete_hides_row_from_later_readers() {
        let env = Env::new(16);
        let t1 = env.txns.begin(TxnMode::ReadWrite);
        let rid = env.store.insert(&t1, HEAP, b"doomed").unwrap();
        t1.commit().unwrap();

        let t2 = env.txns.begin(TxnMode::ReadWrite);
        env.store.delete(&t2, rid).unwrap();
        t2.commit().unwrap();

        let t3 = env.txns.begin(TxnMode::ReadOnly);
        assert_eq!(env.store.read(&t3, rid).unwrap(), None);
        t3.commit().unwrap();
    }

    #[test]
    fn double_update_is_serialization_failure() {
        let env = Env::new(16);
        let t1 = env.txns.begin(TxnMode::ReadWrite);
        let rid = env.store.insert(&t1, HEAP, b"v1").unwrap();
        t1.commit().unwrap();

        // Two concurrent writers; the second to touch the row loses.
        let a = env.txns.begin(TxnMode::ReadWrite);
        let b = env.txns.begin(TxnMode::ReadWrite);
        env.store.update(&a, rid, b"a").unwrap();
        a.commit().unwrap();
        // b's target version is now superseded by a committed txn.
        assert!(matches!(
            env.store.update(&b, rid, b"b"),
            Err(CoreError::SerializationFailure)
        ));
        b.abort().unwrap();
    }

    #[test]
    fn scan_returns_visible_rows_through_eviction() {
        // Tiny pool forces eviction; correctness must survive reload.
        let env = Env::new(2);
        let t = env.txns.begin(TxnMode::ReadWrite);
        let mut expected = Vec::new();
        for i in 0..50u32 {
            let payload = format!("row-{i}").into_bytes();
            env.store.insert(&t, HEAP, &payload).unwrap();
            expected.push(payload);
        }
        // Delete a few.
        // (Re-scan within the same txn to find rids, then delete the first 5.)
        let rows = env.store.scan(&t, HEAP).unwrap();
        for (rid, _) in rows.iter().take(5) {
            env.store.delete(&t, *rid).unwrap();
        }
        t.commit().unwrap();

        let reader = env.txns.begin(TxnMode::ReadOnly);
        let visible_rows = env.store.scan(&reader, HEAP).unwrap();
        assert_eq!(visible_rows.len(), 45, "50 inserted - 5 deleted");
        reader.commit().unwrap();
    }
}
