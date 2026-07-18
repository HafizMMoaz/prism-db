# Component: Lock Manager

**Crate:** `prism-core`
**Status:** Accepted
**Last updated:** 2026-05-15

## Purpose

The lock manager mediates concurrent write access to records. Under MVCC with snapshot isolation, readers never lock; only writers do. The lock manager's job is to ensure that at most one writer at a time can modify any given RID, to detect deadlocks, and to release locks on transaction completion.

## Public interface

```rust
pub struct LockManager { /* opaque */ }

impl LockManager {
    pub fn acquire(&self, txn: TxnId, rid: RecordId, timeout: Duration) -> Result<LockGuard>;
    pub fn release_all(&self, txn: TxnId);
}
```

`LockGuard` is RAII; dropping releases the lock. `release_all` is called by the transaction manager on commit or abort to release every lock held by a transaction (since the manager tracks lock ownership per txn).

## Lock granularity

**Per-RID locks only.** No table-level locks, no page-level locks. Each RID has its own conceptual lock.

We do not implement the lock hierarchy that classical relational systems do (table → page → row, with intent locks). Reasons:
- MVCC means readers don't lock, so the read/write contention that motivates intent locks doesn't apply.
- Schema changes (DDL) are extremely rare in the OLTP workload we target; they can hold a coarse-grained schema lock that blocks all DML on the affected table, simply implemented.

A schema lock is a separate, single, coarse lock per object (table, collection, namespace). Acquired by DDL operations; held briefly. Detailed in `components/sql-engine.md`.

## Lock table

Sharded hash map: `RecordId -> LockState`.

```rust
struct LockState {
    holder: Option<TxnId>,
    waiters: VecDeque<Waiter>,
}

struct Waiter {
    txn: TxnId,
    notify: Arc<parking_lot::Mutex<WaiterState>>,
    cond: Arc<parking_lot::Condvar>,
}

enum WaiterState {
    Waiting,
    Granted,
    DeadlockVictim,
    TimedOut,
}
```

Sharding by `RecordId.hash() % N` (N = 64 or 128) keeps contention low; threads acquiring locks for different RIDs don't serialize on a global mutex.

## Acquire algorithm

```
acquire(txn, rid, timeout):
    shard = lock_table.shard_for(rid)
    shard.lock()
    state = shard.entry(rid).or_default()
    
    if state.holder is None:
        state.holder = txn
        shard.unlock()
        return Ok(LockGuard)
    
    if state.holder == txn:
        // re-entrant; allow
        shard.unlock()
        return Ok(LockGuard)
    
    // Need to wait.
    waiter = Waiter { txn, notify, cond }
    state.waiters.push_back(waiter)
    wait_for_graph.add_edge(txn, state.holder)
    shard.unlock()
    
    // Block.
    notify.lock()
    while notify.state == Waiting:
        if !cond.wait_for(notify, timeout):
            // timeout
            remove_self_from_waiters()
            wait_for_graph.remove_edges(txn)
            return Err(LockTimeout)
    
    if notify.state == DeadlockVictim:
        return Err(Deadlock)
    
    return Ok(LockGuard)
```

When the holder releases, it picks the head waiter, sets its state to `Granted`, makes itself no longer the holder and the waiter the new holder, and notifies the waiter's condvar.

## Wait-for graph

A directed graph: edge from T1 to T2 means T1 is waiting on a lock held by T2. A cycle in the graph is a deadlock.

```rust
struct WaitForGraph {
    edges: DashMap<TxnId, HashSet<TxnId>>,   // txn -> who I'm waiting on
}
```

The graph is updated when waiters are added and removed. A dedicated thread runs deadlock detection periodically.

## Deadlock detection

A dedicated thread, every 100 ms (configurable):

1. Take a coarse snapshot of the wait-for graph.
2. Run a cycle detection (Tarjan's strongly connected components is overkill; a depth-first search from each node looking for back-edges suffices for graphs of typical size).
3. For each cycle found:
   a. Pick a victim: the txn with the highest TxnId (youngest, has done the least work).
   b. Mark the victim's `WaiterState` as `DeadlockVictim`.
   c. Notify its condvar.
   d. The victim wakes up in `acquire`, sees `DeadlockVictim`, returns `Err(Deadlock)`.
   e. The application sees `Deadlock`, aborts the transaction, retries.

The victim selection policy (youngest = victim) is the standard one because the youngest transaction has the least sunk cost. Other policies (smallest write set, lowest priority) are post-v1 considerations.

Detection cost is O(V + E) over the graph. With thousands of active transactions, this is a few microseconds; running every 100 ms is negligible overhead.

## Lock holding across awaits

In an async context, a transaction may hold a lock across an `await`. This is intentional: the lock is associated with the transaction, not with a thread. The lock is released when the transaction commits or aborts, not when the future yields.

The implication: a lock-holder that blocks on a slow I/O may hold the lock for an unexpectedly long time. The deadlock detector still works (the wait-for graph is updated when others queue behind it). The lock wait timeout still works (waiters time out after their configured deadline).

## Lock escalation

We do not escalate to coarser-grained locks under contention. v1 has only per-RID locks. If contention is a problem, the application should restructure to reduce write contention; the engine does not silently shift to a different lock granularity.

## Schema locks

A separate, simpler lock manager handles object-level locks (used by DDL):

```rust
pub struct SchemaLockManager { /* simpler */ }

impl SchemaLockManager {
    pub fn acquire_shared(&self, txn: TxnId, oid: ObjectId) -> Result<SchemaGuard>;
    pub fn acquire_exclusive(&self, txn: TxnId, oid: ObjectId) -> Result<SchemaGuard>;
}
```

- DML (insert, update, delete, select) acquires shared schema locks on every object it touches.
- DDL (create table, drop table, create index) acquires exclusive schema locks.
- Exclusive blocks shared and vice versa.

This is a classical reader-writer pattern at the object level. Schema changes pause DML on affected objects; DML proceeds without contention against other DML.

## Configuration

```toml
[lock_manager]
lock_table_shards = 64
default_wait_timeout_ms = 30000
deadlock_detection_interval_ms = 100
```

## Metrics

- `prism_lock_acquisitions_total`
- `prism_lock_waits_total`
- `prism_lock_wait_duration_seconds` (histogram)
- `prism_lock_timeouts_total`
- `prism_deadlocks_total`
- `prism_lock_holders_active` (gauge)

## Testing

- Unit: every state transition.
- Property: random concurrent lock acquisitions; verify mutual exclusion.
- Deadlock test: deliberately construct cycles of various sizes; verify detection within one cycle of the detector.
- Stress: 1000 txns contending for the same RID; verify no live-lock, all eventually proceed.
- Fairness: under sustained contention, every waiter eventually acquires; no starvation.

## References

- ADR 0004 - MVCC; reads don't lock.
- `components/transaction-manager.md` - releases all locks on commit/abort.
- `components/mvcc.md` - uses the lock manager for write-write conflict prevention.
- Gray and Reuter, *Transaction Processing*, chapter 8.
