//! The transaction manager, transaction handle, snapshots, and commit log.
//!
//! See `docs/components/transaction-manager.md`. Snapshots use the Postgres-style
//! `{xmin, xmax, active-set}` model (race-free) rather than the commit-LSN sketch
//! in `mvcc.md`; the snapshot-isolation semantics are identical. The commit log
//! is in-memory and rebuilt from the WAL by recovery (a later increment).

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use prism_wal::record::RecordPayload;
use prism_wal::{LogRecord, Lsn, Wal};

use crate::error::Result;
use crate::lock::{LockConfig, LockManager};
use crate::{BOOTSTRAP_TXN, FIRST_USER_TXN, TxnId};

/// Isolation/access mode for a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnMode {
    /// May read and write.
    ReadWrite,
    /// Read-only: allocates no undo state and writes no commit record.
    ReadOnly,
}

/// The durable status of a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitStatus {
    /// Allocated but not yet finalized (or unknown).
    InProgress,
    /// Committed; `commit_lsn` is the LSN of its commit record (ZERO for
    /// read-only transactions, which write none).
    Committed {
        /// LSN of the commit record.
        commit_lsn: Lsn,
    },
    /// Aborted.
    Aborted,
}

/// An immutable, snapshot-isolation view of which transactions are visible.
///
/// A transaction `t`'s effects are visible to this snapshot iff `t` is our own
/// id, or (`t < xmax` and `t` is not in `active` and `t` has committed).
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// The owning transaction's id.
    pub txn_id: TxnId,
    /// The smallest transaction id still active when this snapshot was taken
    /// (the vacuum horizon; informational in v1).
    pub xmin: TxnId,
    /// One past the highest transaction id allocated at snapshot time; any
    /// `t >= xmax` began after us and is invisible.
    pub xmax: TxnId,
    /// Transactions in progress at snapshot time — invisible even if they later
    /// commit.
    pub active: HashSet<TxnId>,
}

impl Snapshot {
    /// Construct a snapshot (primarily for tests).
    pub fn new(txn_id: TxnId, xmin: TxnId, xmax: TxnId, active: HashSet<TxnId>) -> Self {
        Self {
            txn_id,
            xmin,
            xmax,
            active,
        }
    }
}

/// The in-memory commit log: `TxnId -> CommitStatus`.
///
/// A single mutex around a hash map in v1; sharding for lock-free reads on the
/// visibility hot path is a documented follow-up.
pub struct CommitLog {
    map: Mutex<HashMap<TxnId, CommitStatus>>,
}

impl CommitLog {
    /// A commit log seeded so the bootstrap transaction is always committed.
    pub fn new() -> Self {
        let mut map = HashMap::new();
        map.insert(
            BOOTSTRAP_TXN,
            CommitStatus::Committed {
                commit_lsn: Lsn::ZERO,
            },
        );
        Self {
            map: Mutex::new(map),
        }
    }

    /// The status of `txn_id` (absent ⇒ `InProgress`).
    pub fn status(&self, txn_id: TxnId) -> CommitStatus {
        self.map
            .lock()
            .expect("commit log poisoned")
            .get(&txn_id)
            .copied()
            .unwrap_or(CommitStatus::InProgress)
    }

    /// Record a transaction as committed (also used by recovery's analysis pass).
    pub(crate) fn record_commit(&self, txn_id: TxnId, commit_lsn: Lsn) {
        self.map
            .lock()
            .expect("commit log poisoned")
            .insert(txn_id, CommitStatus::Committed { commit_lsn });
    }

    /// Record a transaction as aborted (also used by recovery's analysis pass).
    pub(crate) fn record_abort(&self, txn_id: TxnId) {
        self.map
            .lock()
            .expect("commit log poisoned")
            .insert(txn_id, CommitStatus::Aborted);
    }
}

impl Default for CommitLog {
    fn default() -> Self {
        Self::new()
    }
}

/// Owns transaction lifecycle: id allocation, the active set, the commit log,
/// and the lock manager (whose locks it releases on commit/abort).
pub struct TxnManager {
    wal: Arc<Wal>,
    next_txn: AtomicU64,
    active: Mutex<BTreeSet<TxnId>>,
    commit_log: CommitLog,
    locks: LockManager,
}

impl TxnManager {
    /// Create a transaction manager over `wal`, allocating ids from
    /// [`FIRST_USER_TXN`].
    pub fn new(wal: Arc<Wal>) -> Self {
        Self {
            wal,
            next_txn: AtomicU64::new(FIRST_USER_TXN),
            active: Mutex::new(BTreeSet::new()),
            commit_log: CommitLog::new(),
            locks: LockManager::new(LockConfig::default()),
        }
    }

    /// The lock manager (used by the record store to acquire write locks).
    pub fn locks(&self) -> &LockManager {
        &self.locks
    }

    /// Create a manager whose id allocator resumes at `next_txn` (used by
    /// recovery once it has scanned the WAL high-water mark).
    pub fn with_next_txn(wal: Arc<Wal>, next_txn: TxnId) -> Self {
        let m = Self::new(wal);
        m.next_txn
            .store(next_txn.max(FIRST_USER_TXN), Ordering::SeqCst);
        m
    }

    /// The commit log (for the visibility function).
    pub fn commit_log(&self) -> &CommitLog {
        &self.commit_log
    }

    /// The status of a transaction.
    pub fn commit_status(&self, txn_id: TxnId) -> CommitStatus {
        self.commit_log.status(txn_id)
    }

    /// The next id that would be allocated (the current allocation high-water).
    pub fn next_txn_id(&self) -> TxnId {
        self.next_txn.load(Ordering::SeqCst)
    }

    /// Begin a transaction, taking its snapshot.
    pub fn begin(&self, mode: TxnMode) -> TxnHandle<'_> {
        let txn_id = self.next_txn.fetch_add(1, Ordering::SeqCst);

        let snapshot = {
            let mut active = self.active.lock().expect("active set poisoned");
            // `active` excludes us (not inserted yet); concurrently-allocated
            // ids are either here (active) or >= xmax — invisible either way.
            let active_set: HashSet<TxnId> = active.iter().copied().collect();
            let xmax = self.next_txn.load(Ordering::SeqCst);
            let xmin = active.iter().next().copied().unwrap_or(txn_id).min(txn_id);
            active.insert(txn_id);
            Snapshot {
                txn_id,
                xmin,
                xmax,
                active: active_set,
            }
        };

        TxnHandle {
            manager: self,
            txn_id,
            mode,
            snapshot,
            last_lsn: Cell::new(Lsn::ZERO),
            finished: Cell::new(false),
        }
    }

    fn commit_internal(&self, txn_id: TxnId, mode: TxnMode, last_lsn: Lsn) -> Result<()> {
        match mode {
            TxnMode::ReadWrite => {
                let commit_lsn = self.wal.append(LogRecord::txn(
                    txn_id,
                    last_lsn,
                    RecordPayload::Commit {
                        commit_micros: now_micros(),
                        flags: 0,
                    },
                ))?;
                self.wal.flush_through(commit_lsn)?; // the durability point
                self.commit_log.record_commit(txn_id, commit_lsn);
            }
            TxnMode::ReadOnly => {
                self.commit_log.record_commit(txn_id, Lsn::ZERO);
            }
        }
        // Release write locks only after the commit is durable, so blocked
        // writers wake to a committed state.
        self.locks.release_all(txn_id);
        self.active
            .lock()
            .expect("active set poisoned")
            .remove(&txn_id);
        Ok(())
    }

    fn abort_internal(&self, txn_id: TxnId, mode: TxnMode, last_lsn: Lsn) -> Result<()> {
        // NOTE: undo of this transaction's page modifications (reverse-apply +
        // CLRs) is added with the record store. For now abort just records the
        // outcome; a transaction with no writes has nothing to undo.
        if mode == TxnMode::ReadWrite {
            let abort_lsn =
                self.wal
                    .append(LogRecord::txn(txn_id, last_lsn, RecordPayload::Abort))?;
            self.wal.flush_through(abort_lsn)?;
        }
        self.commit_log.record_abort(txn_id);
        self.locks.release_all(txn_id);
        self.active
            .lock()
            .expect("active set poisoned")
            .remove(&txn_id);
        Ok(())
    }
}

/// A handle to an in-progress transaction.
///
/// Held by exactly one thread at a time (`!Sync`, via interior `Cell`s). If
/// dropped without [`TxnHandle::commit`] or [`TxnHandle::abort`], it aborts.
pub struct TxnHandle<'a> {
    manager: &'a TxnManager,
    txn_id: TxnId,
    mode: TxnMode,
    snapshot: Snapshot,
    last_lsn: Cell<Lsn>,
    finished: Cell<bool>,
}

impl TxnHandle<'_> {
    /// This transaction's id.
    pub fn id(&self) -> TxnId {
        self.txn_id
    }

    /// This transaction's mode.
    pub fn mode(&self) -> TxnMode {
        self.mode
    }

    /// This transaction's snapshot.
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// The LSN of this transaction's most recent WAL record (0 if none). The
    /// record store updates this after each append to form the `prev_lsn` chain.
    pub fn last_lsn(&self) -> Lsn {
        self.last_lsn.get()
    }

    /// Record the LSN of this transaction's latest WAL record.
    pub fn set_last_lsn(&self, lsn: Lsn) {
        self.last_lsn.set(lsn);
    }

    /// Commit the transaction durably.
    pub fn commit(self) -> Result<()> {
        self.finished.set(true);
        self.manager
            .commit_internal(self.txn_id, self.mode, self.last_lsn.get())
    }

    /// Abort the transaction.
    pub fn abort(self) -> Result<()> {
        self.finished.set(true);
        self.manager
            .abort_internal(self.txn_id, self.mode, self.last_lsn.get())
    }
}

impl Drop for TxnHandle<'_> {
    fn drop(&mut self) {
        if !self.finished.get() {
            // Best-effort abort of a leaked transaction.
            let _ = self
                .manager
                .abort_internal(self.txn_id, self.mode, self.last_lsn.get());
        }
    }
}

fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_testkit::TempDir;
    use prism_wal::{Config as WalConfig, SyncMode};

    fn manager() -> (TempDir, Arc<Wal>, TxnManager) {
        let tmp = TempDir::new("txn").unwrap();
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
        let mgr = TxnManager::new(wal.clone());
        (tmp, wal, mgr)
    }

    #[test]
    fn ids_are_monotonic_from_first_user_txn() {
        let (_t, _w, mgr) = manager();
        let a = mgr.begin(TxnMode::ReadWrite);
        let b = mgr.begin(TxnMode::ReadWrite);
        assert_eq!(a.id(), FIRST_USER_TXN);
        assert_eq!(b.id(), FIRST_USER_TXN + 1);
        a.abort().unwrap();
        b.abort().unwrap();
    }

    #[test]
    fn commit_and_abort_record_status() {
        let (_t, _w, mgr) = manager();
        let a = mgr.begin(TxnMode::ReadWrite);
        let ai = a.id();
        a.commit().unwrap();
        assert!(matches!(
            mgr.commit_status(ai),
            CommitStatus::Committed { .. }
        ));

        let b = mgr.begin(TxnMode::ReadWrite);
        let bi = b.id();
        b.abort().unwrap();
        assert_eq!(mgr.commit_status(bi), CommitStatus::Aborted);
    }

    #[test]
    fn snapshot_excludes_concurrent_active_txns() {
        let (_t, _w, mgr) = manager();
        let a = mgr.begin(TxnMode::ReadWrite); // id 2
        let b = mgr.begin(TxnMode::ReadWrite); // id 3, concurrent with a
        // a's snapshot was taken before b existed: b is not visible (>= a.xmax).
        assert!(a.snapshot().xmax <= b.id());
        // b's snapshot includes a in its active set (a still running).
        assert!(b.snapshot().active.contains(&a.id()));
        a.abort().unwrap();
        b.abort().unwrap();
    }

    #[test]
    fn drop_without_finalize_aborts() {
        let (_t, _w, mgr) = manager();
        let id;
        {
            let txn = mgr.begin(TxnMode::ReadWrite);
            id = txn.id();
            // dropped here without commit/abort
        }
        assert_eq!(mgr.commit_status(id), CommitStatus::Aborted);
        // No longer active.
        assert!(!mgr.active.lock().unwrap().contains(&id));
    }

    #[test]
    fn concurrent_begin_commit_no_id_reuse() {
        use std::collections::HashSet as Set;
        let (_t, _w, mgr) = manager();
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        let mut ids = Vec::new();
                        for _ in 0..100 {
                            let txn = mgr.begin(TxnMode::ReadWrite);
                            ids.push(txn.id());
                            txn.commit().unwrap();
                        }
                        ids
                    })
                })
                .collect();
            let all: Vec<TxnId> = handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect();
            let unique: Set<TxnId> = all.iter().copied().collect();
            assert_eq!(all.len(), unique.len(), "txn ids were reused");
            assert_eq!(all.len(), 800);
        });
    }
}
