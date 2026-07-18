# ADR 0004: MVCC with snapshot isolation

**Status:** Accepted
**Date:** 2026-05-15

## Context

Concurrency control determines which interleavings of concurrent operations are allowed. Two major families dominate:

1. **Lock-based (pessimistic).** Strict two-phase locking (S2PL) acquires shared locks for reads and exclusive locks for writes; conflicting accesses block. Used by SQL Server (default), DB2, MySQL/InnoDB (in part), most early commercial systems.

2. **Multi-version (optimistic-ish).** Writers create new versions of tuples; readers see a consistent snapshot defined by their start time. Used by PostgreSQL, Oracle, MySQL/InnoDB (also), Spanner, CockroachDB, every newer engine.

Within MVCC, the isolation level offered varies:

- **Read committed:** each statement sees the latest committed data at statement start. Different statements within one transaction may see different states. Postgres default.
- **Snapshot isolation:** the entire transaction sees one snapshot taken at transaction start. Reads never block; writers detect write-write conflicts. Most engines' upper level. Vulnerable to write-skew anomalies.
- **Serializable snapshot isolation (SSI):** snapshot plus extra machinery (read sets, conflict tracking) to detect serialization anomalies. Postgres serializable, CockroachDB serializable.

Cross-model transactions specifically require that the concurrency control mechanism not depend on the data model. A scheme that locks rows in tables doesn't naturally extend to documents and KV pairs; a scheme that versions tuples by TxnId does, because tuples are tuples regardless of model.

## Decision

Prism uses **MVCC with snapshot isolation** for v1.0.

Specifically:
- Each tuple version carries `xmin` (creating txn) and `xmax` (deleting txn).
- Each transaction takes a snapshot at start: the set of TxnIds committed before begin.
- Visibility: a tuple version V is visible to txn T if `V.xmin` is in T's snapshot (committed before T began) and `V.xmax` is either zero or not in T's snapshot.
- Write conflicts: if T attempts to update a tuple whose `xmax` is set by an active (uncommitted) txn, T blocks until that txn finishes; if `xmax` was set by a txn that committed after T's snapshot, T aborts with a serialization failure.
- Read-only transactions never block and never abort.

Serializable isolation (SSI on top of snapshot) is **out of scope for v1**. We accept the write-skew limitation and document it.

## Alternatives considered

### Strict 2PL with deadlock detection
**For:** Simpler conceptually. No version chains. Implementations are well-understood. Provides serializability without extra machinery.

**Against:** Readers block writers and writers block readers. For a workload with long-running reads (analytical queries, document scans, large range scans on KV), throughput collapses under contention. Lock escalation (row → page → table) introduces unfair scheduling and reduces concurrency further.

The cross-model story is also worse: locking a SQL row, a document, and a KV pair in one transaction requires a unified lock manager that can name objects across models. MVCC sidesteps this by versioning at the tuple level - the lock manager only mediates write-write conflicts on the same tuple.

### Snapshot isolation with SSI on top (Postgres-style serializable)
**For:** True serializability. Write-skew anomalies prevented. The right answer for users who demand it.

**Against:** Implementation complexity. SSI requires tracking read sets per transaction and detecting "dangerous structures" (rw-antidependencies that form cycles). Postgres took years to ship this correctly. For a v1 with a small engineering team, this is the wrong battle.

We prefer to ship snapshot isolation correctly and offer SSI as a post-v1 upgrade for users whose workloads need it. The vast majority of OLTP workloads are safe under snapshot isolation; write-skew anomalies are real but uncommon and usually visible in code review.

### OCC (optimistic concurrency control) without versioning
**For:** No locks, no version chains. Transactions read freely, validate at commit, abort on conflict.

**Against:** Validation requires read-set tracking, which is similar in complexity to SSI. Without versioning, readers see in-flight changes from other transactions, which is incorrect. OCC works in practice only with some form of versioning underneath.

### Per-model concurrency control (locks for SQL, lock-free for KV, etc.)
**For:** Each model uses what's best for it.

**Against:** Cross-model transactions become impossible because there is no unified concurrency control. The entire thesis of Prism fails.

## Why snapshot isolation specifically

1. **Reads never block.** Readers walk version chains; writers create new versions. Read-only workloads (which dominate most applications) run at full speed regardless of contention.

2. **Implementation is well-bounded.** xmin/xmax per tuple, a commit log, a snapshot at begin - these are concrete, finite mechanisms. The recovery design (ADR 0003) already requires per-tuple metadata; xmin/xmax fits naturally.

3. **Cross-model uniformity.** A tuple is a tuple; xmin/xmax bookkeeping works identically for SQL rows, documents, and KV pairs. The cross-model property is preserved.

4. **Industry default.** Snapshot isolation is what most applications expect today, whether they call it "READ COMMITTED" (postgres), "MVCC mode" (Mongo), or just "the way it works."

## Visibility logic

Pseudocode:

```
fn visible(version: TupleVersion, snapshot: Snapshot, commits: CommitLog) -> bool {
    if version.xmin == snapshot.txn_id {
        // We created this version ourselves.
        return version.xmax != snapshot.txn_id; // unless we also deleted it
    }
    if !commits.is_committed(version.xmin) {
        return false; // creator hasn't committed
    }
    if commits.commit_lsn(version.xmin) > snapshot.lsn {
        return false; // creator committed after our snapshot
    }
    // version was created before our snapshot
    if version.xmax == 0 {
        return true; // not deleted
    }
    if version.xmax == snapshot.txn_id {
        return false; // we deleted it ourselves
    }
    if !commits.is_committed(version.xmax) {
        return true; // deleter hasn't committed, still visible to us
    }
    if commits.commit_lsn(version.xmax) > snapshot.lsn {
        return true; // deleter committed after our snapshot
    }
    return false; // deleter committed before our snapshot
}
```

Special cases:
- We never see our own aborted writes (the abort cleanup process marks them invisible).
- We see our own in-flight writes (if `version.xmin == snapshot.txn_id`).

## Version chains

When tuple at RID R is updated:

1. New version V' is allocated (typically on the same page if space permits, otherwise on another page).
2. V'.xmin = current txn, V'.xmax = 0.
3. Old version V at R gets V.xmax = current txn.
4. V's "next version" pointer points to V'.
5. Indexes are updated to point at V' (the current version).

A reader following an index to RID R may need to walk the version chain backward to find a version visible to its snapshot.

In v1, version chains are unbounded; old versions accumulate until vacuumed. Vacuum is out of scope for v1 (the engine accumulates dead tuples over time, which is an acknowledged limitation; restart-with-compact is the operational workaround).

## Consequences

### Enabled
- Read-only transactions never block writers.
- Cross-model transactions work uniformly: visibility is per-tuple, not per-model.
- Long-running reads (analytical queries) coexist with short writes (OLTP).
- No deadlock between readers and writers (writers can deadlock with each other; handled by the lock manager).

### Constrained
- Storage overhead per tuple for xmin/xmax (16 bytes).
- Write-skew anomalies are possible. Documented in the user-facing isolation level documentation.
- Dead tuples accumulate; v1 does not vacuum. Users must restart-with-compact periodically.
- The commit log must persist for as long as the oldest active transaction needs to evaluate visibility against committed txns. In practice, retained for a configurable window.

### Required follow-on decisions
- Tuple header layout → `specs/record-format.md`.
- Lock manager for write-write conflicts → `components/lock-manager.md`.
- Version chain pointer format → `specs/record-format.md`.

## References

- Berenson, Bernstein, Gray, Melton, O'Neil, O'Neil: "A Critique of ANSI SQL Isolation Levels." SIGMOD 1995. Defines snapshot isolation and its anomalies.
- Cahill, Röhm, Fekete: "Serializable Isolation for Snapshot Databases." SIGMOD 2008. The SSI paper if we ever want to add it.
- PostgreSQL MVCC documentation.
- ADR 0003 - recovery and MVCC compose; the WAL stores xmin/xmax as part of tuple bytes.
- ADR 0006 - cross-model transactions rely on the model-agnostic property of MVCC.
