# Module Layout

**Status:** Accepted
**Last updated:** 2026-05-15

This document describes the Cargo workspace layout. Module boundaries are normative: they enforce the layering of the architecture by making upward dependencies impossible.

## Workspace structure

```
prism-db/
├── Cargo.toml                  # workspace manifest
├── rust-toolchain.toml         # pinned toolchain
├── crates/
│   ├── prism-storage/          # disk manager, page format, I/O
│   ├── prism-wal/              # write-ahead log
│   ├── prism-buffer/           # buffer pool
│   ├── prism-core/             # transaction manager, record store, MVCC, lock manager
│   ├── prism-index/            # B+tree, hash index
│   ├── prism-sql/              # SQL engine
│   ├── prism-doc/              # document engine
│   ├── prism-kv/               # KV engine
│   ├── prism-protocol/         # wire protocol types and serialization
│   ├── prism-server/           # network server, dispatcher, auth
│   ├── prism-shell/            # interactive shell binary
│   ├── prism-sdk-node/         # napi-rs Node.js SDK
│   ├── prism-fsck/             # offline integrity checker
│   └── prism-bench/            # benchmark harness
└── tools/
    ├── jepsen/                 # Jepsen-style test harness (separate workspace)
    └── fuzz/                   # cargo-fuzz targets
```

## Dependency graph

Arrow A → B reads "A depends on B." No cycles, ever. CI enforces this with `cargo-deny`.

```
                            ┌──────────────────────┐
                            │   prism-shell (bin)  │
                            └──────────┬───────────┘
                                       │
                            ┌──────────▼───────────┐
                            │   prism-protocol     │
                            └──────────────────────┘
                                       ▲
                            ┌──────────┴───────────┐
                            │   prism-server (bin) │
                            └──────────┬───────────┘
                                       │
                ┌──────────────────────┼──────────────────────┐
                │                      │                      │
        ┌───────▼────────┐    ┌────────▼────────┐    ┌────────▼────────┐
        │   prism-sql    │    │   prism-doc     │    │   prism-kv      │
        └───────┬────────┘    └────────┬────────┘    └────────┬────────┘
                │                      │                      │
                └──────────────────────┼──────────────────────┘
                                       │
                            ┌──────────▼───────────┐
                            │   prism-index        │
                            └──────────┬───────────┘
                                       │
                            ┌──────────▼───────────┐
                            │   prism-core         │
                            └──────────┬───────────┘
                                       │
                            ┌──────────▼───────────┐
                            │   prism-buffer       │
                            └──────────┬───────────┘
                                       │
                ┌──────────────────────┴──────────────────────┐
                │                                             │
        ┌───────▼────────┐                          ┌─────────▼────────┐
        │   prism-wal    │                          │  prism-storage   │
        └───────┬────────┘                          └──────────────────┘
                │                                             ▲
                └─────────────────────────────────────────────┘

prism-sdk-node depends on prism-protocol (for types) and FFIs into prism-core via napi-rs
prism-fsck depends on prism-storage and prism-wal only
prism-bench depends on the embedded API (everything via prism-server's library form)
```

## Crate responsibilities

### `prism-storage`
The foundation. No upward dependencies.

- Heap file abstraction
- Page read/write primitives
- `fsync` semantics
- Page format definitions (slotted layout types, header, slot, tuple header)
- Page checksumming
- Direct I/O (`O_DIRECT`) wrapper

**Does not depend on:** anything in Prism.
**External deps:** `bytes`, `crc32fast`, OS file APIs.

### `prism-wal`
Write-ahead log.

- Log record format definitions
- Log writer with group commit
- Log reader for recovery
- LSN allocation
- Log file segmentation and archiving

**Depends on:** `prism-storage`.

### `prism-buffer`
Buffer pool.

- `BufferPool` type with fixed-size frame array
- Page table (`PageId → FrameId`)
- Pin/unpin semantics
- Clock-sweep replacement policy
- Dirty page tracking
- Background page cleaner
- Enforces the WAL invariant (does not flush a page until its log is durable)

**Depends on:** `prism-storage`, `prism-wal`.

### `prism-core`
Transactional record store. The heart of the engine.

- Transaction manager (`TxnId` allocation, commit log, active transaction table)
- MVCC tuple operations (insert, update, delete with xmin/xmax bookkeeping)
- Visibility logic
- Version chain traversal
- Lock manager (per-record locks for writers, wait-for graph, deadlock detection)
- Recovery driver (uses WAL primitives, coordinates analysis/redo/undo)
- Catalog access (system tables)

**Depends on:** `prism-buffer`, `prism-wal`, `prism-storage`.

### `prism-index`
Access methods that are not heap scan.

- B+tree (Lehman-Yao variant for concurrent access)
- Hash index (extendible hashing)
- Common index traits

**Depends on:** `prism-core`.

### `prism-sql`
Relational engine.

- SQL parser (built on `sqlparser-rs`)
- Binder (resolves identifiers against the catalog)
- Logical and physical planner
- Volcano executor with the operators listed in `vision-and-scope.md`
- Type system and expression evaluator

**Depends on:** `prism-core`, `prism-index`.

### `prism-doc`
Document engine.

- Document format encoder/decoder (tagged binary)
- Query predicate parser (Mongo-subset operators)
- Predicate compiler (decides scan vs. index)
- Path expression evaluator for field-based indexes

**Depends on:** `prism-core`, `prism-index`.

### `prism-kv`
Key-value engine.

- Namespace abstraction
- Get/put/delete operations
- Range scan support (if namespace uses ordered index)

**Depends on:** `prism-core`, `prism-index`.

### `prism-protocol`
Wire protocol types. Pure data definitions, no I/O.

- Request and Response enums
- Serialization via a stable binary format (see `specs/wire-protocol.md`)
- Versioning

**Depends on:** nothing in Prism (intentional - both server and clients pull this in).

### `prism-server`
The server binary.

- TCP listener with TLS support
- Connection state machine
- Authentication
- Query dispatcher (routes to engine by request type)
- Implicit and explicit transaction handling
- Library form: exposes an in-process API for embedded use

**Depends on:** `prism-sql`, `prism-doc`, `prism-kv`, `prism-protocol`.

### `prism-shell`
Interactive shell binary.

- Readline-based REPL
- Output formatting per model
- Connection management

**Depends on:** `prism-protocol`.

### `prism-sdk-node`
Node.js SDK.

- napi-rs bindings exposing connection, transaction, and per-model APIs
- TypeScript definitions auto-generated by napi-rs
- Either: pure FFI into the embedded library, or remote client over TCP. Default: remote client.

**Depends on:** `prism-protocol` (and napi-rs).

### `prism-fsck`
Offline integrity checker.

- Validates page checksums
- Validates WAL integrity
- Reports orphaned records, broken version chains, index/heap inconsistencies

**Depends on:** `prism-storage`, `prism-wal` (only the formats, no live state).

### `prism-bench`
Benchmark harness binary.

- Synthetic workloads (insert-heavy, read-heavy, mixed)
- Latency and throughput reporting
- Comparison-friendly output formats

**Depends on:** the embedded API via `prism-server`'s library form.

## Forbidden patterns

- `prism-storage` referencing the WAL. Storage knows nothing about logging; logging is built on top of it.
- `prism-core` referencing any of the three engines. The engines depend on core; core depending on engines would create cycles and defeat the layering.
- Any engine depending on another engine. SQL must not depend on document, etc.
- Any crate referencing `prism-server` directly except `prism-bench`.

## Build profile

- `dev`: optimizations off, debug assertions on, faster compile times.
- `release`: `opt-level = 3`, LTO `thin`, codegen-units = 1, `panic = abort`.
- `bench`: same as release but with frame pointers preserved for profilers.
- `fuzz`: `opt-level = 3`, debug assertions on, ASan / UBSan enabled (where applicable).

## Test layout

Per crate:
- `src/` - implementation
- `tests/` - integration tests
- `benches/` - Criterion benchmarks
- Property-based tests live alongside the modules they test, using `proptest`.

Cross-crate tests (the Jepsen-style harness, the end-to-end shell tests) live in `tools/jepsen/` and `tools/e2e/`, as separate workspaces to keep `cargo test` fast.

## MSRV (Minimum Supported Rust Version)

1.85, Rust edition 2024. Pinned via `rust-toolchain.toml`. Updates require an ADR.
