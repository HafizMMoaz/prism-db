//! The embedded database: the shared storage stack plus the three engines.
//!
//! `Database` assembles one disk manager, WAL, buffer pool, transaction manager,
//! and record store, then layers the SQL, document, and KV engines on top — all
//! sharing that single store, so a transaction spans all three models (the
//! cross-model ACID guarantee). A [`crate::Session`] borrows a `Database` to
//! serve protocol requests.
//!
//! **Scope (this increment):** opens a *fresh* database in a directory.
//! Recovery-on-open and catalog/namespace persistence (so SQL tables and
//! document/KV namespace→heap maps survive restart) are a follow-up — the
//! in-memory maps below are the same class of deferral as the SQL catalog.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use prism_buffer::{BufferPool, Config as BufConfig};
use prism_core::store::{HeapId, RecordStore};
use prism_core::txn::TxnManager;
use prism_doc::DocCollection;
use prism_kv::KvNamespace;
use prism_sql::SqlEngine;
use prism_storage::DiskManager;
use prism_wal::{Config as WalConfig, SyncMode, Wal};

use crate::auth::UserStore;
use crate::error::Result;

// Heap-id ranges, kept disjoint per model so the in-memory registries never
// collide. SQL tables live at 1000.. (allocated by the SQL catalog); documents
// and KV sit far above that.
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
    doc_heaps: Mutex<HashMap<String, HeapId>>,
    doc_next: AtomicU64,
    kv_namespaces: Mutex<HashMap<String, Arc<KvNamespace>>>,
    kv_next: AtomicU64,
}

impl Database {
    /// Open a fresh database under `dir`, creating the heap file and WAL.
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_with(dir, Config::default())
    }

    /// Open a fresh database with explicit [`Config`].
    pub fn open_with(dir: &Path, config: Config) -> Result<Self> {
        let disk = Arc::new(DiskManager::open(&dir.join("heap.db"), true)?);
        let wal = Arc::new(Wal::open(
            &dir.join("wal"),
            WalConfig {
                segment_size: config.wal_segment_size,
                sync_mode: config.wal_sync,
            },
        )?);
        let buffer = Arc::new(BufferPool::new(
            disk,
            wal.clone(),
            BufConfig {
                frame_count: config.buffer_frames,
            },
        )?);
        let txns = Arc::new(TxnManager::new(wal.clone()));
        let store = Arc::new(RecordStore::new(buffer, wal, txns.clone()));
        let sql = SqlEngine::new(store.clone(), txns.clone());
        Ok(Self {
            store,
            txns,
            sql,
            users: UserStore::with_default_admin()?,
            doc_heaps: Mutex::new(HashMap::new()),
            doc_next: AtomicU64::new(DOC_HEAP_BASE),
            kv_namespaces: Mutex::new(HashMap::new()),
            kv_next: AtomicU64::new(KV_HEAP_BASE),
        })
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

    /// A document collection by name, creating its heap on first use.
    pub fn collection(&self, name: &str) -> DocCollection {
        let heap = {
            let mut map = self.doc_heaps.lock().expect("doc heap map poisoned");
            *map.entry(name.to_string())
                .or_insert_with(|| HeapId(self.doc_next.fetch_add(1, Ordering::Relaxed)))
        };
        DocCollection::new(self.store.clone(), heap)
    }

    /// A KV namespace by name, creating it (with its own heap) on first use.
    /// The namespace object is cached so its in-memory key→RID index persists
    /// across requests within this process.
    pub fn kv_namespace(&self, name: &str) -> Arc<KvNamespace> {
        let mut map = self
            .kv_namespaces
            .lock()
            .expect("kv namespace map poisoned");
        map.entry(name.to_string())
            .or_insert_with(|| {
                let heap = HeapId(self.kv_next.fetch_add(1, Ordering::Relaxed));
                Arc::new(KvNamespace::new(self.store.clone(), heap))
            })
            .clone()
    }
}
