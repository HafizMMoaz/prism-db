# Project: Engineering Standards

**Status:** Accepted
**Last updated:** 2026-05-15

These standards govern how we write and review code. They are deliberately opinionated. The goal is consistency: a contributor should be able to read any module in the project and recognize the conventions.

## Language version

Rust 2024 edition, MSRV 1.85 (see `rust-toolchain.toml`).

## Formatting

`rustfmt` with default settings, plus:

```toml
# rustfmt.toml
imports_granularity = "Module"
group_imports = "StdExternalCrate"
newline_style = "Unix"
```

Run `cargo fmt --all` before pushing. CI rejects unformatted code.

## Linting

`cargo clippy --all-targets --all-features -- -D warnings` must pass.

Project-wide allow/deny:

```rust
// Crate roots
#![warn(
    clippy::pedantic,
    clippy::nursery,
    clippy::cargo,
    missing_docs,
)]
#![allow(
    clippy::module_name_repetitions,   // common with Rust style
    clippy::cargo_common_metadata,      // not relevant for binary crates
)]
```

When a lint flags something we genuinely think is fine, use `#[allow]` with a comment explaining why. `#[allow(clippy::...)]` without a comment is rejected in review.

## Error handling

### `Result`, not `panic!`

Use `Result<T, E>` for all recoverable errors. `panic!` only for unreachable cases or invariant violations the compiler can't prove. Recovery code may panic if it cannot proceed - that's the fsync gate pattern.

### Error types per crate

Each crate defines its own error type, typically via `thiserror`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("WAL is full")]
    Full,
    #[error("checksum mismatch at LSN {0}")]
    BadChecksum(Lsn),
}
```

Avoid `anyhow::Error` in library code - it loses type information. Use `anyhow` only at binary boundaries (`main`, tests) where the caller treats every error as opaque.

### `?` is preferred to `match`

Use `?` for error propagation. Only `match` when you actually need to inspect the error.

### Don't swallow errors

Logging an error and continuing is fine when the error is recoverable. Discarding `Result` with `let _ =` is allowed only when truly intentional, and the code must say why.

## Unsafe code

`unsafe` requires:
1. A `// SAFETY: ...` comment immediately above the unsafe block, explaining why the operation is sound.
2. Review by someone other than the author.
3. A test (or property) that exercises the case if any non-trivial reasoning was involved.

We prefer safe code, even when slower. The performance gap is rarely worth the risk.

Examples where unsafe is justified:
- Calling FFI for system calls (`fdatasync` directly via `libc`).
- Pointer arithmetic in performance-critical buffer pool paths (with thorough testing).
- Reinterpreting bytes as page header types (where alignment is verified).

Examples where unsafe is not justified:
- "It would be faster." (Profile first; the compiler is good.)
- "The safe version is awkward." (Refactor.)

## Naming

- Modules: `snake_case`, short, nouns.
- Types: `CamelCase`.
- Functions and methods: `snake_case`, verb phrases.
- Constants: `SCREAMING_SNAKE_CASE`.
- Type parameters: short (`T`, `U`) or descriptive when meaningful (`Key`, `Page`).

Public items have doc comments (enforced by clippy's `missing_docs`). Private items have doc comments when the name doesn't make the purpose obvious.

Avoid abbreviations except for established ones (`txn`, `wal`, `mvcc`, `lsn`).

## Module organization

```
crate/
├── Cargo.toml
├── src/
│   ├── lib.rs              # Public surface only; re-exports
│   ├── module/
│   │   ├── mod.rs          # Public items; private modules below
│   │   ├── inner.rs
│   │   └── helpers.rs
│   └── error.rs
├── tests/                  # Integration tests (use only public API)
└── benches/                # Benchmarks
```

`lib.rs` is short - it declares modules and re-exports the public API. No business logic lives there.

## Visibility

- Default to private.
- `pub(crate)` for cross-module use within a crate.
- `pub` only for items genuinely part of the crate's API.

The public API is what we have committed to maintain backward compatibility on. Don't make things public lightly.

## Type design

### Newtypes

Wrap raw integers in newtypes when they have semantic meaning:

```rust
pub struct PageId(pub u64);
pub struct Lsn(pub u64);
pub struct TxnId(pub u64);
```

Prevents the "I passed a TxnId where a PageId was wanted and the compiler didn't catch it" class of bug.

### Builder pattern

For configurable structs with many optional fields, use a builder:

```rust
let server = Server::builder()
    .bind("0.0.0.0:4444")
    .data_dir("/var/lib/prism")
    .build()?;
```

Not for everything - only when the alternative would be a struct with 6+ public fields or a function with 6+ arguments.

### Avoid `Box<dyn Trait>` in hot paths

Trait objects are fine for the Volcano executor and other places where polymorphism is the point. Avoid them in tight loops or per-byte processing.

### `Arc` discipline

`Arc` is fine for shared ownership of immutable state. For mutable shared state, prefer message passing (channels) over `Arc<Mutex<T>>` when possible. When `Arc<Mutex<T>>` is necessary, document the lock ordering rule.

## Concurrency

### Locks

- Use `parking_lot::Mutex` and `parking_lot::RwLock` over `std::sync` versions (faster, smaller, no poisoning).
- Document lock ordering when there is more than one lock in scope.
- Prefer fine-grained locks; the cost of contention is rarely worth a global lock.
- Never hold a lock across an `.await` unless you've explicitly thought about it.

### Atomic operations

`AtomicU64`, `AtomicBool`, etc. for simple cases. Choose memory ordering deliberately:
- `Relaxed` for counters and statistics.
- `Acquire`/`Release` for synchronization primitives we write.
- `SeqCst` only when truly necessary (rarely).

Comment non-obvious ordering choices.

### Channels

`crossbeam-channel` for synchronous bounded channels; `tokio::sync::mpsc` for async. Avoid unbounded channels - they hide backpressure problems.

## Logging

Use `tracing` macros:

```rust
tracing::info!(txn_id = ?txn.id(), "transaction committed");
tracing::error!(error = %e, page_id = ?page, "page read failed");
```

- Use structured fields, not formatted strings.
- `?` for `Debug`, `%` for `Display`.
- Include enough context to identify the request (txn ID, request ID, connection ID).
- Log at the right level (see `operations/observability.md`).

## Async

Tokio runtime. Sync work that takes more than a few microseconds (page parsing, hash computation) goes in `spawn_blocking` to avoid stalling the executor.

The synchronous core (record store, buffer pool, WAL) is not async - it has explicit blocking calls. Async wrappers live in the network and SDK layers.

## Testing

- Tests live alongside the code (`#[cfg(test)] mod tests`) for unit tests; in `tests/` for integration.
- Tests are deterministic. If a test uses randomness, seed it and log the seed on failure.
- Each test asserts one thing. A failing test should tell you what went wrong, not require investigation.
- Test names describe behavior: `commit_after_crash_recovers_data`, not `test1`.

## Documentation

### Doc comments

```rust
/// Brief one-line summary.
///
/// Longer explanation if needed. Mention invariants, edge cases,
/// and references to design docs.
///
/// # Examples
///
/// ```
/// let lsn = wal.append(record)?;
/// ```
///
/// # Errors
///
/// Returns `WalError::Full` if the segment is full.
pub fn append(&self, record: LogRecord) -> Result<Lsn, WalError> {
    // ...
}
```

Public items have doc comments. Items with non-obvious invariants reference the design doc:

```rust
/// Computes the visibility of a tuple version given the snapshot.
///
/// See `docs/components/mvcc.md` for the full visibility rules.
pub fn visible(version: &RecordHeader, snap: &Snapshot, ...) -> Visibility { ... }
```

### Architecture comments

In modules implementing non-trivial algorithms, the file starts with a comment summarizing the design and pointing at the design doc:

```rust
//! ARIES recovery.
//!
//! Three-phase: analysis, redo, undo. See `docs/components/recovery.md`.
//! The paper: Mohan et al. 1992. Specifically referenced sections:
//! - Analysis: §4.3
//! - Redo: §4.4
//! - Undo: §4.5
```

## Performance discipline

- Don't optimize before measuring. Use `criterion` benchmarks and flamegraphs to identify hot spots.
- After identifying a hot spot, the optimization should have a comment justifying it.
- Avoid premature SIMD, unsafe, or unreadable code in pursuit of speed.
- Memory layout matters more than micro-optimizations: prefer cache-friendly access patterns.

## Dependencies

We are conservative about adding dependencies. Before adding one, ask:
- Is this a core capability or a convenience?
- Is the crate well-maintained (recent activity, version history)?
- What is its dependency footprint?
- Could we write this in 50-200 lines?

A lot of small dependencies cost more than they look like in CI build time and security review burden. We prefer fewer, well-chosen dependencies.

Pinned in workspace `Cargo.toml`, not per-crate, to avoid version conflicts.

## Backwards compatibility

Before v1.0 release: anything can change.

After v1.0 release:
- The SDK's public API: semver.
- The wire protocol: protocol version bumps for breaking changes; older versions supported during a deprecation window.
- The on-disk format: version field in page 0; migration tools for breaking changes.
- Internal Rust crates: no compatibility promise.

## Code review

See `project/code-review-guide.md`.

## Commit messages

Conventional Commits:

```
feat(wal): add group commit window configuration

Allow operators to tune the group commit window via prismd.toml.
Defaults to 1ms.

Closes #42.
```

Types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `perf`.
Scope: typically the crate name.
First line ≤72 chars. Body wraps at 72.

## Misc

- Two-space indents are not allowed.
- Lines wrap at 100 columns.
- No trailing whitespace.
- Files end with a newline.

These are enforced by `rustfmt` and EditorConfig.

## References

- `project/code-review-guide.md` - what reviewers check.
- `operations/build-and-dev.md` - the dev workflow.
- The Rust API Guidelines: https://rust-lang.github.io/api-guidelines/
