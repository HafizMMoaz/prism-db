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
//! surrounding data). User accounts and their privileges are likewise persisted
//! (append-only) to a reserved user heap and reloaded on open.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use prism_buffer::{BufferPool, Config as BufConfig};
use prism_core::recover;
use prism_core::store::{HeapId, RecordStore};
use prism_core::txn::{TxnManager, TxnMode};
use prism_doc::DocCollection;
use prism_kv::KvNamespace;
use prism_sql::SqlEngine;
use prism_storage::{DiskManager, PageId};
use prism_wal::{Config as WalConfig, SyncMode, Wal};

use crate::auth::{Privileges, UserStore};
use crate::catalog::{CatalogEntry, CatalogOp, IndexMeta, ObjectKind, UserEntry, UserOp};
use crate::error::Result;

// Heap-id ranges, kept disjoint per model so the registries never collide. The
// catalog's system heap sits below SQL tables (1000..); documents and KV sit far
// above.
const CATALOG_HEAP: HeapId = HeapId(64);
/// Reserved system heap holding append-only user-account records.
const USER_HEAP: HeapId = HeapId(65);
const DOC_HEAP_BASE: u64 = 1 << 40;
const KV_HEAP_BASE: u64 = 1 << 41;

/// How long a committed transaction's idempotency record is retained.
const IDEMPOTENCY_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// A recorded outcome of an idempotent commit (the `TxnAck` fields to replay).
#[derive(Clone, Copy)]
struct IdemRecord {
    txn_id: u64,
    commit_lsn: u64,
    at: Instant,
}

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

impl Config {
    /// A crash-durable configuration: the WAL fsyncs on commit and on segment
    /// rotation. This is the setting a real server (`prismd`) should run with;
    /// the default favors speed (`SyncMode::None`) for embedded and test use.
    pub fn durable() -> Self {
        Self {
            wal_sync: SyncMode::Fsync,
            ..Self::default()
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
    /// idempotency_key -> the recorded commit outcome (for retry de-duplication).
    idempotency: Mutex<HashMap<u128, IdemRecord>>,
}

impl Database {
    /// Open the database under `dir` with the default [`Config`], recovering and
    /// reloading the catalog if it already exists.
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_with(dir, Config::default())
    }

    /// Open the database under `dir` with an explicit [`Config`].
    pub fn open_with(dir: &Path, config: Config) -> Result<Self> {
        Self::open_inner(dir, config, true)
    }

    /// Open a *data-only* database: no user store is seeded or loaded. Used for
    /// the data databases of a multi-database [`crate::Instance`], whose users
    /// live at the instance level (server-global), not per database.
    pub fn open_data(dir: &Path, config: Config) -> Result<Self> {
        Self::open_inner(dir, config, false)
    }

    fn open_inner(dir: &Path, config: Config, manage_users: bool) -> Result<Self> {
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
            users: UserStore::empty(),
            doc_heaps: Mutex::new(HashMap::new()),
            doc_next: AtomicU64::new(DOC_HEAP_BASE),
            kv_namespaces: Mutex::new(HashMap::new()),
            kv_next: AtomicU64::new(KV_HEAP_BASE),
            persisted_tables: Mutex::new(HashSet::new()),
            idempotency: Mutex::new(HashMap::new()),
            store,
            txns,
        };
        if existing {
            db.load_catalog()?;
        }
        // Load persisted accounts (or seed+persist the default admin). Skipped
        // for data-only databases, whose users live at the instance level.
        if manage_users {
            db.load_or_seed_users()?;
        }
        Ok(db)
    }

    /// Take a checkpoint: flush all dirty pages to disk and fsync. After this,
    /// crash recovery can skip the flushed prefix and only replays the WAL tail
    /// written since. Safe to call periodically or before a clean shutdown.
    pub fn checkpoint(&self) -> Result<()> {
        self.store.checkpoint()?;
        Ok(())
    }

    /// Names of all document collections, sorted (for enumeration / export).
    pub fn collection_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .doc_heaps
            .lock()
            .expect("doc heap map poisoned")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Names of all key–value namespaces, sorted (for enumeration / export).
    pub fn kv_namespace_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .kv_namespaces
            .lock()
            .expect("kv namespace map poisoned")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Look up a non-expired idempotency record by key: the `(txn_id,
    /// commit_lsn)` of the original committed transaction, if any.
    pub fn idempotency_lookup(&self, key: u128) -> Option<(u64, u64)> {
        let mut map = self.idempotency.lock().expect("idempotency map poisoned");
        match map.get(&key) {
            Some(rec) if rec.at.elapsed() < IDEMPOTENCY_WINDOW => {
                Some((rec.txn_id, rec.commit_lsn))
            }
            Some(_) => {
                map.remove(&key); // expired
                None
            }
            None => None,
        }
    }

    /// Record a committed transaction's outcome under `key`, pruning expired
    /// records opportunistically.
    pub fn idempotency_record(&self, key: u128, txn_id: u64, commit_lsn: u64) {
        let mut map = self.idempotency.lock().expect("idempotency map poisoned");
        map.retain(|_, rec| rec.at.elapsed() < IDEMPOTENCY_WINDOW);
        map.insert(
            key,
            IdemRecord {
                txn_id,
                commit_lsn,
                at: Instant::now(),
            },
        );
    }

    /// Create (or replace) a user account with a password and READ+WRITE
    /// privileges.
    pub fn add_user(&self, username: &str, password: &str) -> Result<u64> {
        self.create_user(username, password, Privileges::read_write())
    }

    /// Create (or replace) a user account with explicit privileges, persisting
    /// it so the account survives restart.
    pub fn create_user(
        &self,
        username: &str,
        password: &str,
        privileges: Privileges,
    ) -> Result<u64> {
        let (oid, _phc) = self.users.add_user(username, password, privileges)?;
        self.persist_user_snapshot(username)?;
        Ok(oid)
    }

    /// Set a user's global privileges (`GRANT`/`REVOKE` with no database scope),
    /// persisting the change.
    pub fn set_user_privileges(&self, username: &str, privileges: Privileges) -> Result<()> {
        self.users.set_privileges(username, privileges)?;
        self.persist_user_snapshot(username)
    }

    /// Set a user's privileges for a single database (`GRANT`/`REVOKE … ON <db>`),
    /// persisting the change.
    pub fn set_db_privileges(
        &self,
        username: &str,
        db: &str,
        privileges: Privileges,
    ) -> Result<()> {
        self.users.set_db_privileges(username, db, privileges)?;
        self.persist_user_snapshot(username)
    }

    /// Remove a user account, persisting a tombstone.
    pub fn drop_user(&self, username: &str) -> Result<()> {
        self.users.drop_user(username)?;
        self.persist_user(&UserEntry {
            op: UserOp::Delete,
            username: username.to_string(),
            oid: 0,
            privileges: 0,
            phc: String::new(),
            db_grants: Vec::new(),
        })
    }

    /// Persist the full current state of `username` (global privileges + every
    /// per-database grant) as one append-only `Upsert` record.
    fn persist_user_snapshot(&self, username: &str) -> Result<()> {
        let (oid, phc, privileges, grants) = self
            .users
            .account_snapshot(username)
            .ok_or_else(|| crate::error::ServerError::State(format!("no such user: {username}")))?;
        let db_grants = grants
            .into_iter()
            .map(|(db, p)| (db, p.bits()))
            .collect::<Vec<_>>();
        self.persist_user(&UserEntry {
            op: UserOp::Upsert,
            username: username.to_string(),
            oid,
            privileges: privileges.bits(),
            phc,
            db_grants,
        })
    }

    /// Append a user record to the reserved user heap (in its own transaction).
    fn persist_user(&self, entry: &UserEntry) -> Result<()> {
        let bytes = entry.encode()?;
        let txn = self.txns.begin(TxnMode::ReadWrite);
        match self.store.insert(&txn, USER_HEAP, &bytes) {
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

    /// Load persisted accounts; if none exist (fresh or pre-feature database),
    /// seed and persist the default `admin`.
    fn load_or_seed_users(&self) -> Result<()> {
        if self.load_users()? == 0 {
            self.users.add_user("admin", "admin", Privileges::admin())?;
            self.persist_user_snapshot("admin")?;
        }
        Ok(())
    }

    /// Replay the append-only user records into the in-memory store. Records are
    /// scanned in insertion order (the heap is append-only), so the last
    /// `Upsert` per username wins and a `Delete` removes it. Returns the number
    /// of live accounts loaded.
    fn load_users(&self) -> Result<usize> {
        let reader = self.txns.begin(TxnMode::ReadOnly);
        let records = self.store.scan(&reader, USER_HEAP)?;
        let mut latest: HashMap<String, UserEntry> = HashMap::new();
        for (_rid, payload) in &records {
            let entry = UserEntry::decode(payload)?;
            match entry.op {
                UserOp::Upsert => {
                    latest.insert(entry.username.clone(), entry);
                }
                UserOp::Delete => {
                    latest.remove(&entry.username);
                }
            }
        }
        reader.commit()?;
        let count = latest.len();
        for (username, entry) in latest {
            let grants = entry
                .db_grants
                .iter()
                .map(|(db, bits)| (db.clone(), Privileges::from_bits(*bits)))
                .collect();
            self.users.insert_loaded(
                &username,
                entry.oid,
                entry.phc,
                Privileges::from_bits(entry.privileges),
                grants,
            );
        }
        Ok(count)
    }

    /// The global privileges of the account with `oid`, if any.
    pub fn privileges(&self, oid: u64) -> Option<Privileges> {
        self.users.privileges_of(oid)
    }

    /// The effective privileges of the account with `oid` for database `db`
    /// (a per-database override if present, else the global set).
    pub fn effective_privileges(&self, oid: u64, db: Option<&str>) -> Option<Privileges> {
        self.users.effective_privileges(oid, db)
    }

    /// A user's global privileges and per-database grants (for `SHOW GRANTS`).
    pub fn user_grants(&self, username: &str) -> Option<(Privileges, HashMap<String, Privileges>)> {
        self.users
            .account_snapshot(username)
            .map(|(_, _, privileges, grants)| (privileges, grants))
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
            op: CatalogOp::Upsert,
            kind: ObjectKind::Collection,
            name: name.to_string(),
            heap: heap.0,
            root_page: root.as_u64(),
            primary_key: None,
            columns: vec![],
            indexes: vec![],
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
            op: CatalogOp::Upsert,
            kind: ObjectKind::Namespace,
            name: name.to_string(),
            heap: heap.0,
            root_page: ns.index_root().as_u64(),
            primary_key: None,
            columns: vec![],
            indexes: vec![],
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
                op: CatalogOp::Upsert,
                kind: ObjectKind::Table,
                name: table.name.clone(),
                heap: table.heap.0,
                root_page: table.index_root.map_or(0, |p| p.as_u64()),
                primary_key: table.primary_key.map(|p| p as u32),
                columns: table.columns.clone(),
                indexes: index_metas(&table),
            };
            self.persist_entry(&entry)?;
            persisted.insert(table.name);
        }
        Ok(())
    }

    /// Persist a tombstone for a dropped SQL table. The relational catalog has
    /// already removed it; this records the `DROP` so it does not reappear on
    /// restart, and clears the persisted-name marker so the name may be reused.
    pub fn drop_sql_table(&self, name: &str) -> Result<()> {
        self.persist_entry(&CatalogEntry {
            op: CatalogOp::Delete,
            kind: ObjectKind::Table,
            name: name.to_string(),
            heap: 0,
            root_page: 0,
            primary_key: None,
            columns: vec![],
            indexes: vec![],
        })?;
        self.persisted_tables
            .lock()
            .expect("persisted set poisoned")
            .remove(name);
        Ok(())
    }

    /// Persist a table's current schema as a fresh `Upsert` (for `ALTER TABLE`
    /// add/drop/rename column). The latest record per name wins on reload, so
    /// this overwrites the prior definition.
    pub fn persist_table_schema(&self, name: &str) -> Result<()> {
        let table = self.sql.catalog().table(name)?;
        let indexes = index_metas(&table);
        self.persist_entry(&CatalogEntry {
            op: CatalogOp::Upsert,
            kind: ObjectKind::Table,
            name: name.to_string(),
            heap: table.heap.0,
            root_page: table.index_root.map_or(0, |p| p.as_u64()),
            primary_key: table.primary_key.map(|p| p as u32),
            columns: table.columns,
            indexes,
        })?;
        self.persisted_tables
            .lock()
            .expect("persisted set poisoned")
            .insert(name.to_string());
        Ok(())
    }

    /// Re-key a renamed table in the catalog (`ALTER TABLE … RENAME TO`):
    /// tombstone the old name and persist the schema under the new one.
    pub fn rename_sql_table(&self, old: &str, new: &str) -> Result<()> {
        self.persist_entry(&CatalogEntry {
            op: CatalogOp::Delete,
            kind: ObjectKind::Table,
            name: old.to_string(),
            heap: 0,
            root_page: 0,
            primary_key: None,
            columns: vec![],
            indexes: vec![],
        })?;
        self.persist_table_schema(new)?;
        self.persisted_tables
            .lock()
            .expect("persisted set poisoned")
            .remove(old);
        Ok(())
    }

    /// Drop a document collection: persist a tombstone and forget its mapping,
    /// returning whether it existed. Its heap and `_id` index pages are abandoned
    /// (unreachable) but not reclaimed, as with [`Self::drop_sql_table`].
    pub fn drop_collection(&self, name: &str) -> Result<bool> {
        if !self
            .doc_heaps
            .lock()
            .expect("doc heap map poisoned")
            .contains_key(name)
        {
            return Ok(false);
        }
        self.persist_entry(&CatalogEntry {
            op: CatalogOp::Delete,
            kind: ObjectKind::Collection,
            name: name.to_string(),
            heap: 0,
            root_page: 0,
            primary_key: None,
            columns: vec![],
            indexes: vec![],
        })?;
        self.doc_heaps
            .lock()
            .expect("doc heap map poisoned")
            .remove(name);
        Ok(true)
    }

    /// Drop a key–value namespace: persist a tombstone and forget its mapping,
    /// returning whether it existed. Its heap and index pages are abandoned.
    pub fn drop_namespace(&self, name: &str) -> Result<bool> {
        if !self
            .kv_namespaces
            .lock()
            .expect("kv namespace map poisoned")
            .contains_key(name)
        {
            return Ok(false);
        }
        self.persist_entry(&CatalogEntry {
            op: CatalogOp::Delete,
            kind: ObjectKind::Namespace,
            name: name.to_string(),
            heap: 0,
            root_page: 0,
            primary_key: None,
            columns: vec![],
            indexes: vec![],
        })?;
        self.kv_namespaces
            .lock()
            .expect("kv namespace map poisoned")
            .remove(name);
        Ok(true)
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

        // Replay the append-only catalog: the last record per (kind, name) wins,
        // so a `Delete` tombstone removes an object dropped earlier in the log.
        // Objects are independent, so the surviving entries can register in any
        // order.
        let mut latest: HashMap<(u8, String), CatalogEntry> = HashMap::new();
        for (_rid, payload) in &entries {
            let entry = CatalogEntry::decode(payload)?;
            let key = (entry.kind as u8, entry.name.clone());
            match entry.op {
                CatalogOp::Upsert => {
                    latest.insert(key, entry);
                }
                CatalogOp::Delete => {
                    latest.remove(&key);
                }
            }
        }

        for entry in latest.into_values() {
            let heap = HeapId(entry.heap);
            match entry.kind {
                ObjectKind::Table => {
                    let primary_key = entry.primary_key.map(|p| p as usize);
                    let index_root = primary_key.map(|_| PageId(entry.root_page));
                    let indexes = entry
                        .indexes
                        .iter()
                        .map(|ix| prism_sql::IndexDef {
                            name: ix.name.clone(),
                            column: ix.column as usize,
                            unique: true,
                            root: PageId(ix.root),
                        })
                        .collect();
                    self.sql.catalog().register_table(
                        &entry.name,
                        entry.columns,
                        heap,
                        primary_key,
                        index_root,
                        indexes,
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

/// The persistable form of a table's secondary indexes.
fn index_metas(table: &prism_sql::Table) -> Vec<IndexMeta> {
    table
        .indexes
        .iter()
        .map(|ix| IndexMeta {
            name: ix.name.clone(),
            column: ix.column as u32,
            root: ix.root.as_u64(),
        })
        .collect()
}
