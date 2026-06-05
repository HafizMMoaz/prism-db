# Component: Transaction Manager

**Crate:** `prism-core`
**Status:** Accepted
**Last updated:** 2026-05-15

## Purpose

The transaction manager owns transaction lifecycle: allocating TxnIds, tracking active transactions, recording commits and aborts, and providing the snapshot every transaction reads against. It is a process-wide singleton; every operation across every model funnels through it.

## Public interface

```rust
pub struct TxnManager { /* opaque */ }

impl TxnManager {
    pub fn new(wal: Arc<Wal>, commit_log: CommitLog) -> Self;

    pub fn begin(&self, mode: TxnMode) -> Result<TxnHandle>;
    pub fn commit(&self, txn: &TxnHandle) -> Result<()>;
    pub fn abort(&self, txn: &TxnHandle) -> Result<()>;

    /// Snapshot of currently committed transactions.
    pub fn snapshot(&self) -> Snapshot;

    /// Used by visibility checks.
    pub fn commit_status(&self, txn_id: TxnId) -> CommitStatus;
}

pub enum TxnMode {
    ReadWrite,
    ReadOnly,
}

pub enum CommitStatus {
    InProgress,
    Committed { commit_lsn: Lsn },
    Aborted,
}
```

`TxnHandle` carries the TxnId, the snapshot, an undo-record-list pointer (for abort), and a drop guard that aborts if the handle is dropped without explicit commit/abort.

## TxnId allocation

```rust
struct TxnIdAllocator {
    next: AtomicU64,
}
```

Monotonic, 64-bit. Allocated on `begin`. Never reused. Persisted via the WAL: every Insert/Update/Delete record carries the TxnId, so recovery sees the high-water mark.

At startup, the analysis phase of recovery scans the WAL and initializes `next` to `max_observed_txn_id + 1`.

## Active transaction table

In-memory hash map: `TxnId -> ActiveTxn`.

```rust
struct ActiveTxn {
    txn_id: TxnId,
    mode: TxnMode,
    started_at: Instant,
    snapshot: Snapshot,
    first_lsn: Option<Lsn>,         // for undo chain start
    last_lsn: Option<Lsn>,          // for prev_lsn in next log record
    state: AtomicTxnState,
}

enum TxnState {
    Active,
    Preparing,    // commit in flight
    Committed,
    Aborting,     // undo in flight
    Aborted,
}
```

Read-only txns do not allocate undo metadata; they have no writes to undo.

## Commit log

The commit log maps `TxnId -> CommitStatus` for every transaction that has committed or aborted. It is durable: recovery reconstructs it from the WAL's commit and abort records.

In memory: a sharded hash map for fast lookups.

On disk: implicit. We do not store the commit log as a separate structure. Every commit record in the WAL is the durable evidence; the in-memory map is rebuilt on startup.

Pruning: the commit log can be pruned for TxnIds older than the oldest active transaction's snapshot, but in v1 we never prune; the log grows over the database's lifetime. At 8 bytes per entry, 1 billion transactions = 8 GiB in memory. Acceptable. A pruning pass is a v2 optimization.

## Begin

```
1. txn_id = txn_id_allocator.next()
2. snapshot = build_snapshot()
3. active_txn = ActiveTxn { txn_id, mode, snapshot, state: Active, ... }
4. active_txns.insert(txn_id, active_txn)
5. return TxnHandle { txn_id, snapshot, ... }
```

`build_snapshot` captures:
- `xmin_horizon`: the smallest TxnId of any currently active txn (so older versions can be safely vacuumed; we don't vacuum in v1 but the data is collected).
- `xmax_excluded`: the set of TxnIds that have been allocated but were active at this snapshot time.

A transaction T sees a tuple version V if:
- `V.xmin < T.snapshot.xmax_excluded.min` (committed long before our begin), OR
- `V.xmin` is in T.snapshot.committed_set (committed but after begin? no — see visibility logic in MVCC.md)

The precise visibility logic is in `components/mvcc.md`. The transaction manager's job is to provide a consistent snapshot at begin and to answer commit-status queries during execution.

## Commit

```
1. Set txn.state = Preparing.
2. wal.append(LogRecord::Commit { txn_id: txn.id, ... })  → commit_lsn
3. wal.flush_through(commit_lsn)        ← THE FSYNC
4. commit_log.insert(txn.id, Committed { commit_lsn })
5. Set txn.state = Committed.
6. active_txns.remove(txn.id)
```

After step 3 returns, the transaction is durable. If we crash between 1 and 3, recovery sees no commit record and rolls back. If we crash between 3 and 6, recovery sees the commit record and treats it as committed; the in-memory bookkeeping is rebuilt.

## Abort

```
1. Set txn.state = Aborting.
2. For each WAL record produced by this txn, in reverse prev_lsn order:
   a. Reverse-apply: read before_image, write to page (incrementing page_lsn)
   b. wal.append(LogRecord::Clr { txn, page, slot, undo_image: ... })
3. wal.append(LogRecord::Abort { txn })  → abort_lsn
4. wal.flush_through(abort_lsn) [optional in v1 — abort durability is a correctness nicety, not a requirement; we choose to fsync abort to keep recovery simple]
5. commit_log.insert(txn.id, Aborted)
6. active_txns.remove(txn.id)
```

CLRs make undo idempotent: if we crash during step 2, the next recovery resumes from the last CLR's `undo_next_lsn` field.

## Drop guard

`TxnHandle` implements `Drop`: if it is dropped without `commit()` or `abort()` having been called, the drop calls `abort()`. This catches mistakes — leaked transactions roll back instead of leaking forever.

In Rust this is enforced naturally by ownership: every code path that holds a `TxnHandle` either passes it to `commit`/`abort` (consuming it) or drops it.

## TxnHandle is `!Sync`

A transaction handle is held by exactly one thread at a time. Within Tokio, this means async tasks holding a handle do not pass it across await points to other tasks. This is the simpler concurrency story; supporting cross-thread handles requires a heavyweight handoff protocol we don't need.

In practice, a request handler holds the handle for the duration of the request; if a request spans multiple async operations, all on the same task, the handle is fine.

## Deadlock detection

The transaction manager does not detect deadlocks; the lock manager does. See `components/lock-manager.md`. When the lock manager detects a cycle, it picks a victim and calls `txn_manager.abort(victim)`.

## Concurrency

- `begin`: serialized through the active-txn-table mutex briefly.
- `commit`, `abort`: per-txn state transitions; the txn-table mutex is acquired only for removal at the end.
- `commit_status`: lock-free read of the sharded commit log.
- `snapshot`: brief read lock on active-txn-table to compute `xmin_horizon`.

Hot paths (`commit_status` is called on every tuple visibility check) are lock-free reads.

## Configuration

```toml
[txn]
max_active = 10000           # cap on concurrent transactions
default_isolation = "snapshot"  # only level supported in v1
abort_fsync = true
```

## Metrics

- `prism_txn_active`
- `prism_txn_started_total`
- `prism_txn_committed_total`
- `prism_txn_aborted_total{reason="user"|"deadlock"|"serialization_failure"|"timeout"}`
- `prism_txn_lifetime_seconds` (histogram)

## Testing

- Unit: every state transition.
- Property: random begin/commit/abort sequences; verify the commit log matches expected.
- Concurrent stress: 64 threads, each doing 1000 short transactions; verify no TxnId leaks, no state inconsistencies.
- Recovery: kill during commit, restart, verify the txn is either fully committed or fully rolled back.

## References

- ADR 0004 — MVCC and snapshot isolation.
- ADR 0006 — single TxnManager across models.
- `components/mvcc.md` — visibility logic that consumes the snapshot.
- `components/lock-manager.md` — write-write conflict resolution.
- `components/recovery.md` — analysis phase reconstructs the active txn table.
