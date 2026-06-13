//! `prism-kv` — the key-value engine.
//!
//! Byte-string keys mapped to byte-string values, organized into namespaces. A
//! namespace is a heap of records whose payload is `(key_len, key, value)`
//! ([`docs/specs/record-format.md`]) plus a persistent `key -> RecordId` index.
//! Get/put/delete go through the record store, so MVCC visibility, write locks,
//! and cross-model transactions all apply for free. See
//! `docs/components/kv-engine.md`.
//!
//! The index is a WAL-logged [`prism_index::BTree`], so it is durable: after a
//! restart the namespace reopens at its (fixed) root page — no scan to rebuild an
//! in-memory map. The namespace's heap and its index root are recorded by the
//! catalog so both are found again on open.
//!
//! **Scope (this increment):** point operations. Concurrent writes to the *same*
//! key in one namespace are not yet safe (lookup and index update are separate
//! steps); distinct keys and single-writer-per-key are. Range/scan over the
//! ordered index is a follow-up.

use std::sync::Arc;

use prism_core::RecordId;
use prism_core::error::CoreError;
use prism_core::store::{HeapId, RecordStore};
use prism_core::txn::TxnHandle;
use prism_index::BTree;
use prism_storage::PageId;
use thiserror::Error;

/// Maximum key length, in bytes.
pub const MAX_KEY_SIZE: usize = 1024;

/// Errors produced by the KV engine.
#[derive(Debug, Error)]
pub enum KvError {
    /// An error from the transactional core (MVCC, locks, storage).
    #[error(transparent)]
    Core(#[from] CoreError),

    /// An error from the index (B+tree page I/O, WAL).
    #[error(transparent)]
    Index(#[from] prism_index::IndexError),

    /// The key exceeds [`MAX_KEY_SIZE`].
    #[error("key too large: {size} bytes (max {MAX_KEY_SIZE})")]
    KeyTooLarge {
        /// The offending key length.
        size: usize,
    },

    /// Range/scan was requested on a hash namespace.
    #[error("range/scan is not supported on a hash namespace")]
    RangeNotSupported,
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, KvError>;

/// Encode a KV payload: `u16 key_len | key | value`.
fn encode(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + key.len() + value.len());
    out.extend_from_slice(&(key.len() as u16).to_le_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(value);
    out
}

/// Extract the value bytes from a KV payload.
fn decode_value(payload: &[u8]) -> &[u8] {
    if payload.len() < 2 {
        return &[];
    }
    let key_len = u16::from_le_bytes([payload[0], payload[1]]) as usize;
    payload.get(2 + key_len..).unwrap_or(&[])
}

/// A key-value namespace: a heap of records plus a durable key→RID B+tree index.
pub struct KvNamespace {
    store: Arc<RecordStore>,
    heap: HeapId,
    index: BTree,
}

impl KvNamespace {
    /// Create a new namespace backed by `heap`, allocating a fresh index tree.
    /// The caller persists [`KvNamespace::index_root`] so the tree can be
    /// reopened after restart.
    pub fn create(store: Arc<RecordStore>, heap: HeapId) -> Result<Self> {
        let index = BTree::create(store.buffer(), store.wal())?;
        Ok(Self { store, heap, index })
    }

    /// Reopen an existing namespace whose index tree is rooted at `index_root`.
    pub fn open(store: Arc<RecordStore>, heap: HeapId, index_root: PageId) -> Self {
        let index = BTree::open(store.buffer(), store.wal(), index_root, usize::MAX);
        Self { store, heap, index }
    }

    /// The index tree's (fixed) root page, for the catalog to persist.
    pub fn index_root(&self) -> PageId {
        self.index.root_page()
    }

    /// Every `(key, value)` pair visible to `txn`, by scanning the heap. Used
    /// for export/backup; order is unspecified. MVCC hides deleted/superseded
    /// versions, so each live key appears once.
    pub fn entries(&self, txn: &TxnHandle) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        for (_, payload) in self.store.scan(txn, self.heap)? {
            if payload.len() < 2 {
                continue;
            }
            let key_len = u16::from_le_bytes([payload[0], payload[1]]) as usize;
            let key = payload.get(2..2 + key_len).unwrap_or(&[]).to_vec();
            let value = payload.get(2 + key_len..).unwrap_or(&[]).to_vec();
            out.push((key, value));
        }
        Ok(out)
    }

    fn lookup(&self, key: &[u8]) -> Result<Option<RecordId>> {
        Ok(self.index.search(key)?)
    }

    /// The value for `key` visible to `txn`, or `None` if absent/invisible.
    pub fn get(&self, txn: &TxnHandle, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(rid) = self.lookup(key)? else {
            return Ok(None);
        };
        Ok(self
            .store
            .read(txn, rid)?
            .map(|payload| decode_value(&payload).to_vec()))
    }

    /// Set `key` to `value` (upsert) within `txn`.
    ///
    /// If a version of `key` is currently visible to `txn`, it is updated
    /// (chaining a new version); otherwise a new record is inserted. The index
    /// is repointed at the newest version; stale entries are filtered by MVCC
    /// visibility on read.
    pub fn put(&self, txn: &TxnHandle, key: &[u8], value: &[u8]) -> Result<()> {
        if key.len() > MAX_KEY_SIZE {
            return Err(KvError::KeyTooLarge { size: key.len() });
        }
        let payload = encode(key, value);
        let new_rid = match self.lookup(key)? {
            Some(rid) if self.store.read(txn, rid)?.is_some() => {
                self.store.update(txn, rid, &payload)?
            }
            _ => self.store.insert(txn, self.heap, &payload)?,
        };
        self.index.insert(key, new_rid)?;
        Ok(())
    }

    /// Delete `key` within `txn`. Returns whether a visible value was removed.
    ///
    /// The index entry is intentionally left in place: readers with older
    /// snapshots still see the pre-delete version (the deleting transaction's
    /// `xmax` is invisible to them), and a later `put` re-inserts.
    pub fn delete(&self, txn: &TxnHandle, key: &[u8]) -> Result<bool> {
        let Some(rid) = self.lookup(key)? else {
            return Ok(false);
        };
        if self.store.read(txn, rid)?.is_none() {
            return Ok(false); // not visible to us / already deleted
        }
        self.store.delete(txn, rid)?;
        Ok(true)
    }

    /// Put only if the key has no value visible to `txn`. Returns whether it set.
    pub fn put_if_absent(&self, txn: &TxnHandle, key: &[u8], value: &[u8]) -> Result<bool> {
        if self.get(txn, key)?.is_some() {
            return Ok(false);
        }
        self.put(txn, key, value)?;
        Ok(true)
    }

    /// Set `key` to `new` only if its current visible value equals `expected`.
    pub fn compare_and_set(
        &self,
        txn: &TxnHandle,
        key: &[u8],
        expected: &[u8],
        new: &[u8],
    ) -> Result<bool> {
        match self.get(txn, key)? {
            Some(current) if current == expected => {
                self.put(txn, key, new)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_buffer::{BufferPool, Config as BufConfig};
    use prism_core::txn::{TxnManager, TxnMode};
    use prism_storage::DiskManager;
    use prism_testkit::TempDir;
    use prism_wal::{Config as WalConfig, SyncMode, Wal};

    struct Env {
        ns: KvNamespace,
        txns: Arc<TxnManager>,
        _tmp: TempDir,
    }

    impl Env {
        fn new() -> Self {
            let tmp = TempDir::new("kv").unwrap();
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
                BufferPool::new(disk, wal.clone(), BufConfig { frame_count: 16 }).unwrap(),
            );
            let txns = Arc::new(TxnManager::new(wal.clone()));
            let store = Arc::new(RecordStore::new(buffer, wal, txns.clone()));
            let ns = KvNamespace::create(store, HeapId(1)).unwrap();
            Env {
                ns,
                txns,
                _tmp: tmp,
            }
        }
    }

    fn put(env: &Env, key: &[u8], val: &[u8]) {
        let t = env.txns.begin(TxnMode::ReadWrite);
        env.ns.put(&t, key, val).unwrap();
        t.commit().unwrap();
    }

    fn get(env: &Env, key: &[u8]) -> Option<Vec<u8>> {
        let t = env.txns.begin(TxnMode::ReadOnly);
        let v = env.ns.get(&t, key).unwrap();
        t.commit().unwrap();
        v
    }

    #[test]
    fn put_then_get() {
        let env = Env::new();
        put(&env, b"alpha", b"one");
        assert_eq!(get(&env, b"alpha").as_deref(), Some(&b"one"[..]));
        assert_eq!(get(&env, b"missing"), None);
    }

    #[test]
    fn put_updates_value() {
        let env = Env::new();
        put(&env, b"k", b"v1");
        put(&env, b"k", b"v2");
        assert_eq!(get(&env, b"k").as_deref(), Some(&b"v2"[..]));
    }

    #[test]
    fn empty_value_roundtrips() {
        let env = Env::new();
        put(&env, b"k", b"");
        assert_eq!(get(&env, b"k").as_deref(), Some(&b""[..]));
    }

    #[test]
    fn update_respects_snapshot_isolation() {
        let env = Env::new();
        put(&env, b"k", b"v1");

        // A reader that begins before the update keeps seeing v1, via the
        // version chain reached through the (repointed) index entry.
        let reader = env.txns.begin(TxnMode::ReadOnly);
        assert_eq!(
            env.ns.get(&reader, b"k").unwrap().as_deref(),
            Some(&b"v1"[..])
        );

        put(&env, b"k", b"v2");

        assert_eq!(
            env.ns.get(&reader, b"k").unwrap().as_deref(),
            Some(&b"v1"[..])
        );
        reader.commit().unwrap();

        // A fresh reader sees v2.
        assert_eq!(get(&env, b"k").as_deref(), Some(&b"v2"[..]));
    }

    #[test]
    fn delete_hides_key_but_old_snapshot_still_sees_it() {
        let env = Env::new();
        put(&env, b"k", b"v");

        let reader = env.txns.begin(TxnMode::ReadOnly); // before the delete

        let t = env.txns.begin(TxnMode::ReadWrite);
        assert!(env.ns.delete(&t, b"k").unwrap());
        t.commit().unwrap();

        // New snapshot: gone. Old snapshot: still visible.
        assert_eq!(get(&env, b"k"), None);
        assert_eq!(
            env.ns.get(&reader, b"k").unwrap().as_deref(),
            Some(&b"v"[..])
        );
        reader.commit().unwrap();

        // Deleting a missing/already-deleted key returns false.
        let t = env.txns.begin(TxnMode::ReadWrite);
        assert!(!env.ns.delete(&t, b"k").unwrap());
        assert!(!env.ns.delete(&t, b"nope").unwrap());
        t.commit().unwrap();
    }

    #[test]
    fn conditional_ops() {
        let env = Env::new();
        let t = env.txns.begin(TxnMode::ReadWrite);
        assert!(env.ns.put_if_absent(&t, b"k", b"first").unwrap());
        assert!(!env.ns.put_if_absent(&t, b"k", b"second").unwrap());
        assert!(
            env.ns
                .compare_and_set(&t, b"k", b"first", b"third")
                .unwrap()
        );
        assert!(
            !env.ns
                .compare_and_set(&t, b"k", b"WRONG", b"fourth")
                .unwrap()
        );
        assert_eq!(
            env.ns.get(&t, b"k").unwrap().as_deref(),
            Some(&b"third"[..])
        );
        t.commit().unwrap();
    }

    #[test]
    fn kv_survives_restart_without_rescan() {
        use prism_core::recover;
        use prism_storage::DiskManager as Disk;

        let tmp = TempDir::new("kv-restart").unwrap();
        let heap_path = tmp.path().join("heap.db");
        let wal_path = tmp.path().join("wal");
        let heap = HeapId(1);
        let wal_cfg = WalConfig {
            segment_size: 256 * 1024,
            sync_mode: SyncMode::None,
        };

        // Session 1: write committed data, capture the index root, then crash.
        let root = {
            let disk = Arc::new(Disk::open(&heap_path, true).unwrap());
            let wal = Arc::new(Wal::open(&wal_path, wal_cfg).unwrap());
            let buffer =
                Arc::new(BufferPool::new(disk, wal.clone(), BufConfig { frame_count: 8 }).unwrap());
            let txns = Arc::new(TxnManager::new(wal.clone()));
            let store = Arc::new(RecordStore::new(buffer, wal, txns.clone()));
            let ns = KvNamespace::create(store, heap).unwrap();

            let t = txns.begin(TxnMode::ReadWrite);
            ns.put(&t, b"k1", b"v1").unwrap();
            ns.put(&t, b"k2", b"v2").unwrap();
            ns.put(&t, b"gone", b"x").unwrap();
            ns.delete(&t, b"gone").unwrap();
            t.commit().unwrap();
            ns.index_root()
        };

        // Recover the heap from the WAL (rebuilds both the data and index pages).
        let wal = Arc::new(Wal::open(&wal_path, wal_cfg).unwrap());
        let report = {
            let disk = Disk::open(&heap_path, false).unwrap();
            let r = recover(&wal, &disk).unwrap();
            disk.close().unwrap();
            r
        };
        let disk = Arc::new(Disk::open(&heap_path, false).unwrap());
        let buffer =
            Arc::new(BufferPool::new(disk, wal.clone(), BufConfig { frame_count: 8 }).unwrap());
        let txns = Arc::new(TxnManager::new_recovered(
            wal.clone(),
            report.next_txn_id,
            &report.committed,
            &report.aborted,
        ));
        let store = Arc::new(RecordStore::new(buffer, wal, txns.clone()));
        store.seed_heap_directory(&report.heaps);

        // Reopen at the persisted index root — no rebuild scan.
        let ns = KvNamespace::open(store, heap, root);
        let reader = txns.begin(TxnMode::ReadOnly);
        assert_eq!(ns.get(&reader, b"k1").unwrap().as_deref(), Some(&b"v1"[..]));
        assert_eq!(ns.get(&reader, b"k2").unwrap().as_deref(), Some(&b"v2"[..]));
        assert_eq!(
            ns.get(&reader, b"gone").unwrap(),
            None,
            "deleted key stays deleted"
        );
        reader.commit().unwrap();
    }

    #[test]
    fn oversized_key_rejected() {
        let env = Env::new();
        let t = env.txns.begin(TxnMode::ReadWrite);
        let big = vec![0u8; MAX_KEY_SIZE + 1];
        assert!(matches!(
            env.ns.put(&t, &big, b"v"),
            Err(KvError::KeyTooLarge { .. })
        ));
        t.abort().unwrap();
    }
}
