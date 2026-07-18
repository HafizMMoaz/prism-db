# Component: MVCC

**Crate:** `prism-core`
**Status:** Accepted
**Last updated:** 2026-05-15

## Purpose

MVCC (Multi-Version Concurrency Control) implements snapshot isolation by storing multiple versions of each tuple and choosing the right version for each reader. It is the consumer of the transaction manager's snapshots and the producer of new versions when writers run.

This module is the implementation of ADR 0004. The architectural rationale is there; this document is the operational detail.

## Public interface

```rust
pub struct RecordStore { /* opaque */ }

impl RecordStore {
    /// Insert a new record. Returns the assigned RecordId.
    pub fn insert(&self, txn: &TxnHandle, hint: HeapHint, payload: &[u8]) -> Result<RecordId>;

    /// Read the version of the record visible to `txn`.
    pub fn read(&self, txn: &TxnHandle, rid: RecordId) -> Result<Option<RecordVersion>>;

    /// Update the record. May produce a new RecordId if the new payload doesn't fit.
    pub fn update(&self, txn: &TxnHandle, rid: RecordId, payload: &[u8]) -> Result<RecordId>;

    /// Mark the record deleted (set xmax).
    pub fn delete(&self, txn: &TxnHandle, rid: RecordId) -> Result<()>;

    /// Iterate all visible records in a heap.
    pub fn scan(&self, txn: &TxnHandle, hint: HeapHint) -> Result<impl Iterator<Item = Result<(RecordId, RecordVersion)>>>;
}
```

`HeapHint` identifies which heap (table OID, collection OID, namespace OID) the record belongs to. The record store routes to the right heap.

`RecordVersion` is a borrowed view: header + payload bytes.

## Tuple lifecycle

### Insert
1. Buffer pool: fetch a writable page in the target heap (typically a page with free space, tracked by free space map).
2. Allocate a slot. Compute record bytes:
   - Header: `xmin = txn.id`, `xmax = 0`, `next_version = NIL`.
   - Payload as provided.
3. Write to page; mark dirty.
4. WAL: append `Insert { txn, page, slot, after_image: bytes }`.
5. Update page.page_lsn to the LSN.
6. Update transaction's prev_lsn chain.
7. Return `(page_id, slot_id) = RecordId`.

### Read (visibility check)
Given RID:
1. Buffer pool: fetch the page for reading.
2. Read the record at slot. Parse header.
3. Run the visibility function (below).
4. If visible: copy payload bytes out (or return a guarded reference).
5. If not visible: walk the version chain via `next_version` pointer.
6. If chain exhausted: return None.

### Visibility function
```
fn visible(version: &RecordHeader, snapshot: &Snapshot, commits: &CommitLog) -> Visibility {
    // Did we create this version?
    if version.xmin == snapshot.txn_id {
        if version.xmax == snapshot.txn_id { return Invisible; }       // we deleted it
        if version.xmax == 0 { return Visible; }                       // alive, ours
        // xmax is someone else? impossible in v1: we hold a write lock on this row
        return Visible;
    }

    // Has xmin committed before our snapshot?
    match commits.status(version.xmin) {
        InProgress => return Invisible,
        Aborted => return Invisible,
        Committed { commit_lsn } => {
            if commit_lsn > snapshot.lsn { return Invisible; }  // committed after us
        }
    }

    // xmin is visible. Now check xmax.
    if version.xmax == 0 { return Visible; }
    if version.xmax == snapshot.txn_id { return Invisible; }   // we deleted it
    match commits.status(version.xmax) {
        InProgress => return Visible,                          // deleter not committed
        Aborted => return Visible,                             // delete rolled back
        Committed { commit_lsn } => {
            if commit_lsn > snapshot.lsn { return Visible; }   // deleted after us
            return Invisible;                                   // deleted before us
        }
    }
}
```

### Update
1. Look up the visible version of the row.
2. Acquire a write lock on the RID (lock manager).
   - If lock held by another active txn: block (or fail-fast if the txn has `wait_timeout`).
   - If lock held by a txn that committed after our snapshot: abort with `SerializationFailure`.
3. Construct the new version:
   - Header: `xmin = txn.id`, `xmax = 0`, `next_version = NIL`.
   - Payload as provided.
4. Try to insert the new version in the same page. If it fits:
   - New RID is `(same_page, new_slot)`.
5. Otherwise, insert in any other page with space.
6. Mark the old version's header: `xmax = txn.id`, `next_version = new_rid`.
7. WAL: append `Update { txn, page, slot, before_image: old_header, after_image: new_payload }` for the new version and a corresponding header-only record for the old version's xmax update.
   - In practice, we write one composite log record covering both modifications, since they must replay atomically.
8. Update affected indexes (insert new index entries pointing at new_rid; old entries remain and are filtered by visibility).

### Delete
1. Look up the visible version.
2. Acquire write lock (same as update).
3. Set `xmax = txn.id` on the version header.
4. WAL: append `Delete { txn, page, slot, before_image: header_with_xmax_0 }`.
5. The payload remains; only the header changes.

## Version chain walking

When a read finds a record but its visible version is not the current version:

```
visible_version_for(rid, snapshot):
    current = fetch(rid)
    if visible(current, snapshot) { return current }
    while current.next_version != NIL:
        current = fetch(current.next_version)
        if visible(current, snapshot) { return current }
    return None
```

**The chain walks backward in time.** When we update, the *old* version's `next_version` points to the *new* version. So actually:

```
visible_version_for(rid, snapshot):
    current = fetch(rid)             // index points at newest version
    if visible(current, snapshot) { return current }
    while current.has_older_version():
        current = fetch_older_version(current)
        if visible(current, snapshot) { return current }
    return None
```

The pointer name is poorly chosen if we say "next_version" but mean "older version." For clarity in this codebase: the field stores the **previous** version's RID; the index always points at the newest version. So a fresh insert: index → new_rid → header has no prev_version. After update: index → new_rid → header.prev_version = old_rid → header has no prev_version. The MVCC chain walks backward through `prev_version`.

This matches Postgres's semantics, where `t_ctid` on an old tuple points forward to its successor; we use the opposite direction because it simplifies snapshot evaluation (we always start at the newest version and walk back, instead of starting at the oldest and walking forward).

The unified record format (ADR 0005) calls this field `next_version` for historical reasons; the on-disk format reserves a name choice. **The semantics are "previous version" in time.** This will be renamed in a future doc revision.

## Write conflict detection

Two writers updating the same RID:

- Writer A: starts at txn 100, locks RID, updates.
- Writer B: starts at txn 101 (after A but A not committed), tries to lock RID.
- B blocks on the lock.
- A commits at LSN 5000.
- B acquires the lock.
- B re-reads the record. The current version's xmin = 100. Was 100 committed before B's snapshot? B's snapshot is from before A committed. So B sees the old version, not A's new version. But B's view of the latest visible version may now be stale.

The first-committer-wins rule: B, upon acquiring the lock, must re-check. If the latest version has `xmin` that committed after B's snapshot, B aborts with `SerializationFailure`. The application retries.

This is the standard SI write-conflict semantic. It is correct but it does aboard transactions under contention; clients must handle retries.

## Lost-update prevention

A naive `UPDATE x = x + 1` read by B before A commits could lose A's increment. Snapshot isolation prevents this via the first-committer-wins rule above: B's update would see the latest version was modified after B's snapshot and abort. This is the only correctness consequence of snapshot isolation; write-skew anomalies are the famous limitation.

## Heap-only tuples (HOT)

Postgres has an optimization called HOT: if an update doesn't change any indexed column, the new version is placed in the same page as the old, and indexes need not be updated; the old version's `t_ctid` points at the new version, and index lookups walk the chain.

**v1 does not implement HOT.** Every update inserts new index entries. Indexes accumulate dead entries between vacuums. Acceptable for v1; an obvious v2 optimization.

## Index entries and MVCC

An index entry is `(key, rid)`. It is not MVCC-aware: index entries are not version-tagged. After an index lookup, the executor fetches the record at the RID and runs the visibility check. Stale entries (pointing at versions that are no longer the newest) are filtered out at that point.

Consequence: indexes grow over time even on read-mostly workloads if writes happen. Vacuum (post-v1) removes dead index entries.

## Tombstones

Deleted records are not removed from pages; their `xmax` is set. The space is reclaimable only when no active transaction could possibly see them (i.e., `xmax`'s commit LSN < oldest active snapshot's LSN). Reclamation is the vacuum process's job; not implemented in v1.

## Configuration

```toml
[mvcc]
lock_wait_timeout_ms = 30000      # default 30 seconds
deadlock_detection_interval_ms = 100
```

## Metrics

- `prism_mvcc_version_chain_walks_total` (histogram by walk length)
- `prism_mvcc_serialization_failures_total`
- `prism_mvcc_visible_records_total`
- `prism_mvcc_invisible_records_total`

## Testing

- Unit: visibility function over a matrix of (xmin, xmax, snapshot, commit_status) combinations.
- Property: random insert/update/delete sequences across multiple txns; verify the final state matches a model implementation.
- Anomaly testing: Elle / Jepsen-style anomaly detection. We expect write skew to be observable; we expect lost updates, dirty reads, and unrepeatable reads to be impossible.
- Stress: long version chains (many updates to one row); verify chain walks terminate.

## References

- ADR 0004 - snapshot isolation choice.
- ADR 0005 - unified record format and xmin/xmax bookkeeping.
- ADR 0006 - single TxnManager and WAL underpin this.
- `components/transaction-manager.md` - snapshot provider.
- `components/lock-manager.md` - write-write conflict resolution.
- Berenson et al. 1995 - isolation level definitions.
- PostgreSQL MVCC.
