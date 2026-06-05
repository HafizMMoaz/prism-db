# ADR 0006: Single WAL and single transaction manager for cross-model transactions

**Status:** Accepted
**Date:** 2026-05-15

## Context

The defining feature of Prism is ACID across data models. A user must be able to write:

```
BEGIN
  INSERT INTO orders ...     -- relational
  db.events.insertOne(...)   -- document
  PUT cache:user:123 = ...   -- key-value
COMMIT
```

and have either all three changes commit atomically or none of them. After a crash, recovery must restore exactly that all-or-nothing property.

There are two ways to implement this:

1. **Per-model transaction managers coordinated by a higher-level protocol.** Each model has its own WAL and transaction manager. Cross-model transactions are implemented via two-phase commit (2PC) over the per-model managers.

2. **Single shared WAL and transaction manager.** All three models route their mutations through one transaction manager and one log. Cross-model transactions are not a special case; they are ordinary transactions whose operations happen to span access methods.

This is the architectural fork that defines the project.

## Decision

Prism uses **one transaction manager and one WAL** shared by all three access methods. Cross-model transactions are implemented by the structural property that there is nothing to cross — operations across models route through the same transaction.

## Alternatives considered

### Per-model logs with 2PC
The 2PC version goes like this:

- Each model has its own WAL.
- A coordinator (the dispatcher) drives 2PC: prepare each model, collect votes, broadcast commit or abort.
- Recovery: each model recovers its own log, then a separate recovery phase resolves in-doubt transactions using the coordinator's log.

**Against:** This is more complexity than the project can afford. 2PC introduces the in-doubt window, requires a coordinator log separate from the model logs, complicates recovery substantially, and produces well-known availability problems if the coordinator dies. Production 2PC implementations (XA transactions in JEE, Postgres prepared transactions) are notorious for operational difficulty.

More fundamentally, 2PC is the right design when the participants are independent systems that must remain independent (cross-database transactions across separately-administered Postgres clusters, for example). Inside one process, with shared memory and a shared disk, 2PC is solving a problem we don't have.

### Per-model logs without 2PC
Each model has its own log; cross-model writes are simply not atomic.

**Against:** This abandons the project thesis. If we ship without cross-model atomicity, we are a polyglot wrapper, not a multi-model database.

### Single WAL but per-model transaction managers
One log, three transaction managers. Each TM allocates its own TxnIds; cross-model coordination via some shared registry.

**Against:** TxnId uniqueness across models requires a global allocator, at which point we have one TxnManager again. Per-model TMs also fragment the active transaction table, the commit log, and the snapshot semantics; visibility logic that works across models becomes a coordination problem.

## How the chosen design works

### One TxnManager

A single `TxnManager` is instantiated at server startup. It owns:

- The TxnId counter (monotonic, 64-bit).
- The active transaction table (in-memory hash map: TxnId → state).
- The commit log (durable mapping: TxnId → committed / aborted / in-progress + commit LSN).

All three access methods call into this one manager to begin, commit, and abort transactions.

### One WAL

A single `Wal` instance writes log records from all three access methods. Records are tagged with TxnId (and indirectly, by the operation type, with the model). The log writer doesn't care which model produced the record; it just appends.

### Cross-model transaction lifecycle

```
1. Client (or implicit machinery) calls dispatcher.begin()
   dispatcher.begin() calls txn_manager.begin() → returns TxnHandle(txn_id=42)
   
2. Client issues SQL insert:
   sql_engine.execute(txn=42, "INSERT INTO orders ...")
     → record_store.insert(txn=42, payload=row_bytes, hint=Heap(orders))
       → wal.append(LogRecord::Insert { txn: 42, page: P1, slot: S1, after: row_bytes })
       → page P1 marked dirty
   
3. Client issues document insert:
   doc_engine.execute(txn=42, db.events.insertOne(...))
     → record_store.insert(txn=42, payload=doc_bytes, hint=Heap(events))
       → wal.append(LogRecord::Insert { txn: 42, page: P2, slot: S2, after: doc_bytes })
       → page P2 marked dirty
   
4. Client issues KV put:
   kv_engine.execute(txn=42, PUT cache:user:123 = ...)
     → record_store.insert(txn=42, payload=kv_bytes, hint=Heap(cache_ns))
       → wal.append(LogRecord::Insert { txn: 42, page: P3, slot: S3, after: kv_bytes })
       → page P3 marked dirty
       → hash_index.insert(cache_ns, key, RID(P3, S3))
         → wal.append(LogRecord::IndexInsert { ... txn: 42 ... })
   
5. Client calls dispatcher.commit()
   dispatcher.commit() calls txn_manager.commit(42)
     → wal.append(LogRecord::Commit { txn: 42 })
     → wal.flush_through(commit_lsn)  ← single fsync covers all four operations
     → commit_log[42] = COMMITTED
```

At step 5, the WAL contains five records (three Inserts, one IndexInsert, one Commit), all tagged with TxnId 42, all flushed together. The atomicity invariant is: all of them are durable, or none of them are durable (because the Commit record is the last). If the system crashes after step 4 and before step 5's fsync returns, recovery sees no Commit record for txn 42 and treats 42 as a loser; undo rolls back all four operations using the same machinery used for any aborted transaction.

If the system crashes after step 5's fsync returns and before the client receives the response: the transaction is committed. The client must retry with the same idempotency key; the server returns the existing result.

### Why this is simpler, not just more elegant

The shared-WAL design is **less code, not more.** Per-model logs would require:
- Three WAL implementations (or one templated over three contexts).
- A coordinator log on top.
- 2PC state machine.
- Cross-log recovery.
- A way to keep the per-model TxnId spaces coordinated.

The single-WAL design has none of these. The WAL is the WAL; the access methods don't know they're sharing it. The cost of the unification is: we had to commit to a unified record format (ADR 0005). That cost paid for itself the moment we wrote down the cross-model lifecycle above.

## Consequences

### Enabled
- True cross-model atomicity, by construction.
- One commit fsync per transaction regardless of model count.
- One recovery code path for all crash scenarios.
- One visibility logic for all reads.
- Implementation simplicity: cross-model is not a feature, it is the default.

### Constrained
- A WAL writer bottleneck across all writes. Hot. Group commit and a dedicated writer thread make this acceptable for v1; lock-free log writers are a v2 optimization.
- The transaction manager is a single point of failure inside the process. Mitigated by the fact that the entire process is a single point of failure (this is a single-node engine).
- All three models pay the same MVCC overhead per record (24 bytes). Documented in ADR 0005.

### Required follow-on
- Group commit configuration → `components/wal.md`.
- TxnId allocation granularity (per call vs. batched) → `components/transaction-manager.md`.

## References

- ADR 0003 — WAL design.
- ADR 0004 — MVCC isolation.
- ADR 0005 — unified record format.
- Postgres's WAL is unary in this sense (one log per cluster). MongoDB's journal likewise. There is precedent.
- Gray and Reuter, *Transaction Processing*, on why 2PC is the wrong tool for one-process transactions.
