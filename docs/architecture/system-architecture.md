# System Architecture

**Status:** Accepted
**Last updated:** 2026-05-15

## Overview

Prism is a layered system. Each layer depends only on the layers below it. The crucial property of the architecture is that all three access methods — SQL, document, KV — are siblings at the same layer, sitting on top of one shared transactional record store. Nothing in the storage or transaction layer knows what data model is being served.

## Layer diagram

```
                       ┌──────────────────────────────────┐
                       │           Clients                │
                       │   (SDK / Shell / Embedded API)   │
                       └────────────┬─────────────────────┘
                                    │ TCP binary protocol
                       ┌────────────▼─────────────────────┐
                       │       Network Server             │
                       │  (auth, session, framing)        │
                       └────────────┬─────────────────────┘
                                    │
                       ┌────────────▼─────────────────────┐
                       │       Query Dispatcher           │
                       │  (routes by model, parses)       │
                       └─┬───────────┬───────────┬────────┘
                         │           │           │
                ┌────────▼──┐  ┌─────▼─────┐ ┌──▼────────┐
                │ SQL       │  │ Document  │ │ KV        │
                │ Engine    │  │ Engine    │ │ Engine    │
                │           │  │           │ │           │
                │ parser    │  │ predicate │ │ namespace │
                │ planner   │  │ compiler  │ │ ops       │
                │ executor  │  │ executor  │ │           │
                └────────┬──┘  └─────┬─────┘ └──┬────────┘
                         │           │           │
                ┌────────▼───────────▼───────────▼────────┐
                │       Access Method Layer               │
                │  B+tree, Hash index, Heap scan          │
                └────────────┬────────────────────────────┘
                             │
                ┌────────────▼────────────────────────────┐
                │       Transactional Record Store        │
                │  RecordId → bytes                       │
                │  MVCC visibility, lock manager          │
                └────────────┬────────────────────────────┘
                             │
                ┌────────────▼────────────────────────────┐
                │       Transaction Manager               │
                │  TxnId allocation, commit log           │
                └────────────┬────────────────────────────┘
                             │
        ┌────────────────────┼────────────────────────────┐
        │                    │                            │
   ┌────▼─────┐      ┌───────▼───────┐          ┌─────────▼────┐
   │ Buffer   │◄────►│   WAL         │          │   Catalog    │
   │ Pool     │      │  (log writer, │          │  (system     │
   │          │      │   group       │          │   tables)    │
   │          │      │   commit)     │          │              │
   └────┬─────┘      └───────┬───────┘          └──────────────┘
        │                    │
   ┌────▼─────┐      ┌───────▼───────┐
   │ Disk     │      │ WAL Files     │
   │ Manager  │      │               │
   └────┬─────┘      └───────────────┘
        │
   ┌────▼─────┐
   │  Heap    │
   │  File    │
   └──────────┘
```

## Layer responsibilities

### Disk manager
Owns the heap file. Reads and writes pages by page ID. Provides `fsync` semantics. Does not interpret page contents. See `components/disk-manager.md`.

### Buffer pool
Caches pages in memory. Translates `PageId → Frame`. Manages pin counts, dirty bits, replacement policy (clock sweep). Enforces the WAL invariant: a dirty page cannot be written to disk until the WAL record describing its modification is durable. See `components/buffer-pool.md`.

### WAL
Append-only log of every page mutation. Allocates LSNs. Coordinates group commit. Provides recovery primitives (analysis scan, redo apply, undo apply). See `components/wal.md` and ADR 0003.

### Transaction manager
Allocates `TxnId`s. Maintains the active transaction table. Owns the commit log. Provides `begin`, `commit`, `abort` primitives. See `components/transaction-manager.md`.

### Transactional record store
The fundamental abstraction. Exposes:
- `insert_record(txn, bytes) -> RecordId`
- `read_record(txn, rid) -> Option<bytes>` (returns the version visible to txn)
- `update_record(txn, rid, bytes) -> RecordId`
- `delete_record(txn, rid)`

Handles MVCC visibility: a read with snapshot S returns the version V such that `V.xmin <= S` and (`V.xmax == 0` or `V.xmax > S`) and `V.xmin` is committed. Handles write conflict detection.

This is the layer all three access methods share. There is one of these, not three. See `components/mvcc.md`.

### Access method layer
B+tree, hash index, and heap scan. Each access method maps from its key space to `RecordId` and uses the record store for fetches.
- B+tree: `Key -> RecordId`, supports point and range
- Hash index: `Key -> RecordId`, supports point only
- Heap scan: iterates over all RIDs in a heap (no key)

See `components/btree.md` and `components/hash-index.md`.

### Engine layer
The three access methods (SQL, document, KV) each compile user operations into sequences of access method calls.
- SQL engine: parser → logical plan → physical plan → Volcano executor that emits access method calls
- Document engine: parses query predicate, picks index if available, scans or seeks via access method
- KV engine: thin wrapper, mostly direct hash/btree calls

See `components/sql-engine.md`, `components/document-engine.md`, `components/kv-engine.md`.

### Query dispatcher
Receives parsed requests from the network layer, identifies the target engine by model, routes the operation, handles transaction lifecycle for non-explicit transactions.

### Network server
TCP server. Length-prefixed binary framing. Authentication. Per-session transaction tracking. See `components/network-server.md` and `specs/wire-protocol.md`.

## Cross-cutting concerns

### Logging
Every component uses the `tracing` crate. Structured JSON output. Log levels: `error`, `warn`, `info`, `debug`, `trace`. Production default is `info`.

### Metrics
Every component exposes Prometheus metrics through a central registry. Counter and histogram conventions defined in `operations/observability.md`.

### Error handling
Every component defines its own error type. Errors propagate up the layer stack via `Result`; conversion is explicit (no `?` across layer boundaries without an intermediate `From` impl). User-facing errors are mapped at the engine boundary to a stable, documented error catalog.

### Configuration
Single TOML config file. Components read their own section. Hot-reload is not supported in v1; config changes require restart.

## What "one engine" means concretely

The defining architectural property is that the record store, transaction manager, buffer pool, and WAL are singletons within a Prism process. There is one of each, shared across all three access methods. A transaction touching a SQL table and a document collection:

1. Calls `txn = TxnManager.begin()` — gets a TxnId
2. SQL insert: SQL engine compiles to `RecordStore.insert(txn, row_bytes)`
3. Document update: document engine compiles to `RecordStore.update(txn, rid, doc_bytes)`
4. Calls `TxnManager.commit(txn)` — writes one commit record to the WAL

The WAL contains records for both operations, both tagged with the same TxnId. Recovery replays both. Visibility logic applies the same way to both. The record store has no special path for cross-model transactions because, from its point of view, there is no such thing as a cross-model transaction; there is just a transaction with records that happened to be interpreted by different access methods.

This is the architecture in one sentence: **a transactional record store with three access methods on top.**

## Module boundaries in code

The Cargo workspace mirrors the architecture. Crate dependency direction is downward only:

```
prism-server      depends on  prism-sql, prism-doc, prism-kv
prism-shell       depends on  prism-protocol
prism-sdk-node    depends on  prism-protocol, prism-core (via FFI)
prism-protocol    standalone (wire format only)
prism-sql         depends on  prism-core
prism-doc         depends on  prism-core
prism-kv          depends on  prism-core
prism-core        depends on  prism-wal, prism-buffer, prism-storage
prism-wal         depends on  prism-storage
prism-buffer      depends on  prism-storage
prism-storage     depends on  nothing (the foundation)
```

`prism-core` is where the transactional record store, transaction manager, lock manager, and MVCC live. The three engine crates depend on it, not on each other.

## Threading model

- **Network server:** one Tokio runtime, async I/O for all connections.
- **Query execution:** async; long-running operations yield at access method boundaries.
- **Buffer pool:** synchronous; latches are `parking_lot::RwLock`. No async inside the buffer pool because page access is hot-path and async overhead is not free.
- **WAL writer:** dedicated OS thread for fsync to avoid blocking Tokio executors. Group commit batches arrivals.
- **Recovery:** synchronous, single-threaded. Runs at startup, completes before the server accepts connections.

This is detailed in `components/network-server.md` and `components/wal.md`.

## What is deliberately not shown

The architecture does not show specific persistence layouts (those are in `specs/`), recovery state machines (those are in `components/recovery.md`), or the lifecycle of a single query (that is in `architecture/data-flow.md`). This document is the map; those are the territory.
