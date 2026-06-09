//! The embedded database: the shared storage stack plus the three engines.
//!
//! `Database` assembles one disk manager, WAL, buffer pool, transaction manager,
//! and record store, then layers the SQL, document, and KV engines on top — all
//! sharing that single store, so a transaction spans all three models (the
//! cross-model ACID guarantee). A [`crate::Session`] borrows a `Database` to
//! serve protocol requests.
//!
//! **Durability.** Each named object (SQL table, document collection, KV
//! namespace) is recorded in a persistent catalog (a reserved system heap; see
//! [`crate::catalog`]). [`Database::open`] recovers the record store from the WAL
//! and reloads the catalog, so all three models survive restart. Catalog writes
//! commit in their own transaction (DDL is not yet transactional with
//! surrounding data); user accounts are not yet persisted (re-seeded each start).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use prism_buffer::{BufferPool, Config as BufConfig};
use prism_core::recover;
use prism_core::store::{HeapId, RecordStore};
use prism_core::txn::{TxnManager, TxnMode};
use prism_doc::DocCollection;
use prism_kv::KvNamespace;
use prism_sql::SqlEngine;
use prism_storage::{DiskManager, PageId};
use prism_wal::{Config as WalConfig, SyncMode, Wal};

use crate::auth::UserStore;
use crate::catalog::{CatalogEntry, ObjectKind};
use crate::error::Result;

// Heap-id ranges, kept disjoint per model so the registries never collide. The
// catalog's system heap sits below SQL tables (1000..); documents and KV sit far
// above.
const CATALOG_HEAP: HeapId = HeapId(64);
const DOC_HEAP_BASE: u64 = 1 << 40;
const KV_HEAP_BASE: u64 = 1 << 41;

/// Tuning for the storage stack a [`Database`] builds.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// WAL segment size in bytes.
    pub wal_segment_size: u32,
    /// WAL sync mode.
    pub wal_sync: SyncMode,
    /// Buffer-pool frame count.
    pub buffer_frames: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wal_segment_size: 16 * 1024 * 1024,
            wal_sync: SyncMode::None,
            buffer_frames: 1024,
        }
    }
}

/// An embedded PrismDB instance: the shared engine behind the wire protocol.
pub struct Database {
    store: Arc<RecordStore>,
    txns: Arc<TxnManager>,
    sql: SqlEngine,
    users: UserStore,
    /// Document collection name -> (heap, `_id` index root).
    doc_heaps: Mutex<HashMap<String, (HeapId, PageId)>>,
    doc_next: AtomicU64,
    kv_namespaces: Mutex<HashMap<String, Arc<KvNamespace>>>,
    kv_next: AtomicU64,
    /// SQL table names already written to the persistent catalog.
    persisted_tables: Mutex<HashSet<String>>,
}

impl Database {
    /// Open the database under `dir` with the default [`Config`], recovering and
    /// reloading the catalog if it already exists.
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_with(dir, Config::default())
    }

    /// Open the database under `dir` with an explicit [`Config`].
    pub fn open_with(dir: &Path, config: Config) -> Result<Self> {
        let heap_path = dir.join("heap.db");
        let wal_cfg = WalConfig {
            segment_size: config.wal_segment_size,
            sync_mode: config.wal_sync,
        };
        let buf_cfg = BufConfig {
            frame_count: config.buffer_frames,
        };
        let existing = heap_path.exists();

        let (store, txns) = if existing {
            // Recover the heap from the WAL, then build a store seeded with the
            // rebuilt heap directory and commit log.
            let wal = Arc::new(Wal::open(&dir.join("wal"), wal_cfg)?);
            let report = {
                let disk = DiskManager::open(&heap_path, false)?;
                let report = recover(&wal, &disk)?;
                disk.close()?;
                report
            };
            let disk = Arc::new(DiskManager::open(&heap_path, false)?);
            let buffer = Arc::new(BufferPool::new(disk, wal.clone(), buf_cfg)?);
            let txns = Arc::new(TxnManager::new_recovered(
                wal.clone(),
                report.next_txn_id,
                &report.committed,
                &report.aborted,
            ));
            let store = Arc::new(RecordStore::new(buffer, wal, txns.clone()));
            store.seed_heap_directory(&report.heaps);
            (store, txns)
        } else {
            let disk = Arc::new(DiskManager::open(&heap_path, true)?);
            let wal = Arc::new(Wal::open(&dir.join("wal"), wal_cfg)?);
            let buffer = Arc::new(BufferPool::new(disk, wal.clone(), buf_cfg)?);
            let txns = Arc::new(TxnManager::new(wal.clone()));
            let store = Arc::new(RecordStore::new(buffer, wal, txns.clone()));
            (store, txns)
        };

        let db = Self {
            sql: SqlEngine::new(store.clone(), txns.clone()),
            users: UserStore::with_default_admin()?,
            doc_heaps: Mutex::new(HashMap::new()),
            doc_next: AtomicU64::new(DOC_HEAP_BASE),
            kv_namespaces: Mutex::new(HashMap::new()),
            kv_next: AtomicU64::new(KV_HEAP_BASE),
            persisted_tables: Mutex::new(HashSet::new()),
            store,
            txns,
        };
        if existing {
            db.load_catalog()?;
        }
        Ok(db)
    }

    /// Create (or replace) a user account with a password.
    pub fn add_user(&self, username: &str, password: &str) -> Result<u64> {
        self.users.add_user(username, password)
    }

    /// Verify a username/password, returning the user's OID on success.
    pub fn verify_user(&self, username: &str, password: &str) -> Option<u64> {
        self.users.verify(username, password)
    }

    /// The shared transaction manager.
    pub fn txns(&self) -> Arc<TxnManager> {
        self.txns.clone()
    }

    /// The shared record store.
    pub fn store(&self) -> Arc<RecordStore> {
        self.store.clone()
    }

    /// The relational engine (owns the catalog).
    pub fn sql(&self) -> &SqlEngine {
        &self.sql
    }

    /// A document collection by name, creating (and persisting) its heap and
    /// `_id` index on first use.
    pub fn collection(&self, name: &str) -> Result<DocCollection> {
        let mut map = self.doc_heaps.lock().expect("doc heap map poisoned");
        if let Some((heap, root)) = map.get(name) {
            return Ok(DocCollection::open(self.store.clone(), *heap, *root));
        }
        let heap = HeapId(self.doc_next.fetch_add(1, Ordering::Relaxed));
        let coll = DocCollection::create(self.store.clone(), heap)?;
        let root = coll.index_root();
        self.persist_entry(&CatalogEntry {
            kind: ObjectKind::Collection,
            name: name.to_string(),
            heap: heap.0,
            root_page: root.as_u64(),
            primary_key: None,
            columns: vec![],
        })?;
        map.insert(name.to_string(), (heap, root));
        Ok(coll)
    }

    /// A KV namespace by name, creating (and persisting) it on first use. The
    /// namespace is cached so its in-memory key→RID index persists across
    /// requests within this process.
    pub fn kv_namespace(&self, name: &str) -> Result<Arc<KvNamespace>> {
        let mut map = self
            .kv_namespaces
            .lock()
            .expect("kv namespace map poisoned");
        if let Some(ns) = map.get(name) {
            return Ok(ns.clone());
        }
        let heap = HeapId(self.kv_next.fetch_add(1, Ordering::Relaxed));
        // Create the durable index tree first, then record its root so the
        // namespace reopens without a rescan after restart.
        let ns = Arc::new(KvNamespace::create(self.store.clone(), heap)?);
        self.persist_entry(&CatalogEntry {
            kind: ObjectKind::Namespace,
            name: name.to_string(),
            heap: heap.0,
            root_page: ns.index_root().as_u64(),
            primary_key: None,
            columns: vec![],
        })?;
        map.insert(name.to_string(), ns.clone());
        Ok(ns)
    }

    /// Persist any SQL tables not yet recorded in the catalog. Called after a
    /// `CREATE TABLE` succeeds.
    pub fn persist_sql_tables(&self) -> Result<()> {
        let mut persisted = self
            .persisted_tables
            .lock()
            .expect("persisted set poisoned");
        for table in self.sql.catalog().tables_snapshot() {
            if persisted.contains(&table.name) {
                continue;
            }
            let entry = CatalogEntry {
                kind: ObjectKind::Table,
                name: table.name.clone(),
                heap: table.heap.0,
                root_page: table.index_root.map_or(0, |p| p.as_u64()),
                primary_key: table.primary_key.map(|p| p as u32),
                columns: table.columns.clone(),
            };
            self.persist_entry(&entry)?;
            persisted.insert(table.name);
        }
        Ok(())
    }

    /// Append `entry` to the catalog heap in its own committed transaction.
    fn persist_entry(&self, entry: &CatalogEntry) -> Result<()> {
        let bytes = entry.encode()?;
        let txn = self.txns.begin(TxnMode::ReadWrite);
        match self.store.insert(&txn, CATALOG_HEAP, &bytes) {
            Ok(_) => {
                txn.commit()?;
                Ok(())
            }
            Err(e) => {
                let _ = txn.abort();
                Err(e.into())
            }
        }
    }

    /// Reload the catalog from the system heap after recovery: register tables,
    /// repopulate the document/KV maps, rebuild KV indexes, and advance the heap
    /// allocators past what is already in use.
    fn load_catalog(&self) -> Result<()> {
        let reader = self.txns.begin(TxnMode::ReadOnly);
        let entries = self.store.scan(&reader, CATALOG_HEAP)?;

        let mut doc_heaps = self.doc_heaps.lock().expect("doc heap map poisoned");
        let mut kv_namespaces = self
            .kv_namespaces
            .lock()
            .expect("kv namespace map poisoned");
        let mut persisted = self
            .persisted_tables
            .lock()
            .expect("persisted set poisoned");
        let mut next_doc = DOC_HEAP_BASE;
        let mut next_kv = KV_HEAP_BASE;

        for (_rid, payload) in &entries {
            let entry = CatalogEntry::decode(payload)?;
            let heap = HeapId(entry.heap);
            match entry.kind {
                ObjectKind::Table => {
                    let primary_key = entry.primary_key.map(|p| p as usize);
                    let index_root = primary_key.map(|_| PageId(entry.root_page));
                    self.sql.catalog().register_table(
                        &entry.name,
                        entry.columns,
                        heap,
                        primary_key,
                        index_root,
                    )?;
                    persisted.insert(entry.name);
                }
                ObjectKind::Collection => {
                    doc_heaps.insert(entry.name, (heap, PageId(entry.root_page)));
                    next_doc = next_doc.max(entry.heap + 1);
                }
                ObjectKind::Namespace => {
                    // Reopen the durable index tree at its persisted root — no
                    // rescan to rebuild an in-memory map.
                    let ns = KvNamespace::open(self.store.clone(), heap, PageId(entry.root_page));
                    kv_namespaces.insert(entry.name, Arc::new(ns));
                    next_kv = next_kv.max(entry.heap + 1);
                }
            }
        }

        self.doc_next.store(next_doc, Ordering::SeqCst);
        self.kv_next.store(next_kv, Ordering::SeqCst);
        drop((doc_heaps, kv_namespaces, persisted));
        reader.commit()?;
        Ok(())
    }
}
