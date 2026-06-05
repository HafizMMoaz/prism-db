# ADR 0001: Use Rust as the implementation language

**Status:** Accepted
**Date:** 2026-05-15
**Deciders:** Engineering team

## Context

A storage engine implementing WAL, recovery, buffer pool, MVCC, and three access methods is a systems project measured in tens of thousands of lines of code. Three classes of programming language are plausible candidates:

1. **C** — the language of SQLite and PostgreSQL.
2. **C++** — the language of MySQL, MongoDB, RocksDB, ClickHouse, DuckDB, ScyllaDB.
3. **Rust** — the language of newer engines: TiKV, Materialize, RisingWave, Databend, Neon, ReadySet, sled, fjall.

Higher-level languages (Go, Java, OCaml, Zig) were dismissed early. Go's GC is incompatible with the latency targets of a buffer pool hot path. Java has the same problem plus a runtime dependency that complicates embedded use. OCaml lacks the systems ecosystem. Zig is too immature for a project of this scope; we are not in the business of pioneering a language.

The decision among C, C++, and Rust is the meaningful one.

## Decision

We implement Prism in **Rust**, edition 2024, MSRV 1.85.

## Alternatives considered

### C
**For:** Maximum transparency, minimal abstraction between source and assembly, the language of the most successful single-file embedded database (SQLite) and the oldest production RDBMS (PostgreSQL). Best FFI story bar none — every language can call C.

**Against:** Manual memory management is the wrong primitive for this codebase. The classes of bugs C makes easy — use-after-free on evicted pages, double-free on transaction abort, data races on the buffer pool free list — are precisely the bugs that haunt from-scratch storage engines. Lack of generics forces either macro-heavy code or void-pointer-and-cast patterns that lose type safety where we need it most (the page format types, the WAL record types). No RAII means pin counts and lock guards must be tracked by convention, not the compiler.

SQLite gets away with C because Hipp has worked on it for over twenty years with a test suite of hundreds of millions of cases. Postgres gets away with C because of a thirty-year evolved coding style and aggressive use of memory contexts. We do not have that runway.

### C++
**For:** RAII solves the pin-count, lock-guard, transaction-cleanup problem. Templates allow zero-cost generic operators in execution. The dominant language for production DBMS engines in the post-2000 era. Mature toolchain.

**Against:** The same memory-safety problems as C remain — use-after-free is a compile-checking only by convention (e.g., via `std::unique_ptr` plus `gsl::span` plus discipline). The build system (CMake or Bazel) is a separate large project on top of writing the database. Templates are powerful but error messages are notoriously poor. Concurrency requires careful annotation; data races compile silently.

Modern C++ (20/23) is substantially better than C++11, but the language remains a multi-paradigm compromise where any large codebase ends up using a controlled subset that varies per organization.

### Rust
**For:** Memory safety without garbage collection — the single property that eliminates the worst category of DBMS bugs as compile errors. Send/Sync trait bounds make concurrency invariants checkable. Cargo eliminates the build system as a project. Modern toolchain: `rust-analyzer`, `clippy`, `rustfmt`, `cargo test`, `cargo bench`, `cargo fuzz`, `cargo miri`. Excellent FFI for the Node.js SDK via `napi-rs`. Ecosystem is small but high-quality for systems: `tokio`, `parking_lot`, `crossbeam`, `serde`, `tracing`, `bincode`, `proptest`. Rust is now the chosen language for almost every database engine started in the last decade.

**Against:** The ownership model is genuinely awkward for the data structures DBMS code is built from. B+trees with parent pointers, doubly-linked buffer-pool free lists, buffer-pool frames handed out as mutable references to multiple callers — these are designed-against by the borrow checker. The standard mitigations (arena allocators, index-based references like `FrameId(u32)` instead of `&mut Frame`, judicious `unsafe`) work, but they are a tax. Compile times on a large workspace can become annoying; CI cycles measured in minutes, not seconds. The ecosystem is small compared to C++; certain mature C++ libraries (e.g., RocksDB itself) have no equivalent.

## Why Rust wins for this project

1. **Recovery correctness.** ARIES is hard to implement correctly; the dominant failure modes are subtle state-tracking bugs (e.g., a dirty page table inconsistency, a CLR written for the wrong txn). Eliminating use-after-free and data races at the compiler level removes a whole category of failures that would otherwise contaminate recovery testing.

2. **Concurrency tractability.** As soon as the engine moves past a single global mutex, the matrix of possible thread interleavings explodes. C++ catches races at test time; Rust catches them at compile time via Send/Sync. For a small team without a 24/7 monitoring budget, this is a force multiplier.

3. **SDK story.** The Node.js SDK is a first-class deliverable. `napi-rs` is mature, ergonomic, and generates TypeScript definitions automatically. The C++ equivalent (`node-addon-api` plus `node-gyp`) is functional but markedly more painful.

4. **Selection bias of the field.** Every database engine started since approximately 2015 with serious engineering behind it has chosen Rust or has had public conversations about regretting C++. That signal is not dispositive but it is real.

## Consequences

### Enabled
- Compile-time prevention of memory safety bugs.
- Compile-time prevention of data races on `Sync` types.
- Single toolchain for build, test, format, lint, fuzz, benchmark.
- Clean napi-rs path for the Node.js SDK.
- Ability to use mature ecosystem crates (`tokio`, `serde`, `bincode`, `parking_lot`, `crossbeam`).

### Constrained
- The buffer pool, B+tree, and any data structure with non-tree topology will use index-based references and arena allocation, not direct references. This is documented in component design docs.
- `unsafe` will be used in performance-critical or borrow-checker-incompatible paths. Every `unsafe` block requires a `// SAFETY:` comment explaining the invariant.
- The team will need fluency in Rust's async model for the network layer and synchronous model for the storage layer. These are different mental models within one codebase.
- Compile times will be managed by workspace splitting and `cargo check` for fast feedback.

### Excluded
- Compatibility with C++ template-metaprogramming-heavy execution engines (e.g., DuckDB's expression compiler). We accept this; we are not building an analytical engine.
- Interop with C ABIs other than what we explicitly choose to expose (the FFI surface is minimal: just what napi-rs needs).

## References

- ADR 0009 (SDK strategy) — depends on this decision.
- `architecture/module-layout.md` — workspace structure assumes Cargo.
- Rust reference: <https://doc.rust-lang.org/reference/>
- napi-rs: <https://napi.rs/>
- prior art: TiKV, Materialize, sled, Neon (all Rust, all storage engines).
