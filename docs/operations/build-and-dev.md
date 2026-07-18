# Operations: Build and Development

**Status:** Accepted
**Last updated:** 2026-05-15

This document describes how to build, run, and develop Prism. It targets contributors and operators.

## Toolchain

The project uses a single pinned toolchain via `rust-toolchain.toml`:

```toml
[toolchain]
channel = "1.85.0"
components = ["clippy", "rustfmt"]
profile = "minimal"
```

(`rust-src` and `rust-analyzer` are editor conveniences; install them locally if
your IDE needs them. They are not required to build, lint, or test.)

Rustup users get this automatically. CI verifies the exact version. No other Rust version is supported for the build.

### System dependencies

| Dependency | Reason | Install |
|---|---|---|
| Linux 5.10+ / macOS 12+ / Windows 10+ | platform durable-`fsync` and direct-I/O semantics | OS |
| pkg-config (Linux/macOS) | crate build scripts | `apt install pkg-config` / `brew install pkg-config` |
| protobuf-compiler | for metrics protobufs | `apt install protobuf-compiler` / `brew install protobuf` / `choco install protoc` |
| MSVC Build Tools (Windows) | linker for the `*-pc-windows-msvc` target | Visual Studio Build Tools (C++ workload) |

TLS uses `rustls` (no system OpenSSL needed) so the default build has no native
crypto dependency on any platform.

All three operating systems are supported, first-class targets - for the server,
the SDK, and the shell. Platform-specific file I/O lives behind the storage trait
in `prism-storage` (`components/disk-manager.md`); everything above that layer is
portable Rust.

## Repository layout

```
prism/
├── Cargo.toml                Workspace manifest
├── rust-toolchain.toml
├── docs/                     This documentation
├── crates/
│   ├── prism-storage/
│   ├── prism-buffer/
│   ├── prism-wal/
│   ├── prism-core/
│   ├── prism-index/
│   ├── prism-sql/
│   ├── prism-doc/
│   ├── prism-kv/
│   ├── prism-protocol/
│   ├── prism-server/
│   ├── prism-shell/
│   ├── prism-sdk-node/
│   ├── prism-fsck/
│   └── prism-bench/
├── tests/                    Cross-crate integration tests
├── benches/                  Benchmark workloads
└── tools/
    ├── jepsen/               Fault injection harness
    └── workloads/            Synthetic workload generators
```

Every crate has its own `Cargo.toml`, `src/`, and `tests/`. Inter-crate dependencies are declared explicitly; there is no transitive sharing.

## Building

```bash
# Debug build (fast iteration)
cargo build

# Release build (production)
cargo build --release

# Specific binary
cargo build --release -p prism-server --bin prismd
cargo build --release -p prism-shell --bin prism-shell
```

The release build enables LTO and codegen-units=1; takes about 5-7 minutes on modern hardware. Debug builds rebuild incrementally in seconds.

### Compilation profiles

`Cargo.toml` declares three profiles:

```toml
[profile.dev]
opt-level = 0
debug = true

[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
debug = "line-tables-only"   # Symbols for production debugging without size blowup

[profile.bench]
opt-level = 3
lto = "thin"                 # Faster than fat LTO for benchmark iteration
codegen-units = 1
debug = true
```

A `release-with-debug` profile is available for production binaries that need full debug info.

## Running locally

The server has a single binary:

```bash
# Initialize a new database
mkdir /tmp/prism-data
./target/release/prismd init --data-dir /tmp/prism-data

# Start it
./target/release/prismd run --data-dir /tmp/prism-data --config prismd.toml
```

Sample `prismd.toml`:

```toml
[server]
data_dir = "/tmp/prism-data"
bind = "127.0.0.1:4444"
log_level = "info"

[buffer_pool]
size_mib = 256

[wal]
segment_size_mib = 16
```

In another terminal:

```bash
./target/release/prism-shell --host=localhost --user=admin --database=test
```

The first run prompts for an admin password and creates the admin user.

## Cargo commands

```bash
cargo check                       # Type-check everything fast
cargo clippy --all-targets        # Lints
cargo fmt --all                   # Formatting (CI checks; do this before pushing)
cargo test                        # All unit + integration tests
cargo test -p prism-wal           # One crate
cargo test --test recovery        # One integration test file
cargo test -- --nocapture         # Show stdout from tests
cargo bench                       # Run benchmarks (see operations/benchmarking.md)
```

Convenience cargo aliases in `.cargo/config.toml`:

```toml
[alias]
ci = "test --all-targets --all-features --locked"
lint = "clippy --all-targets --all-features -- -D warnings"
```

## Pre-commit checks

Recommended workflow before pushing:

```bash
cargo fmt --all
cargo lint
cargo test
```

CI runs the same on every PR plus the long-running integration suite, the benchmark regression check, and (for tagged branches) the fault-injection harness.

## Generating documentation

```bash
cargo doc --no-deps --open       # Rust-side API docs
```

This documentation tree (the docs/ directory) is plain Markdown; render with any static site generator or read in-place. No build step required.

## IDE setup

`rust-analyzer` works out of the box with the workspace. Suggested editor settings:

- Format on save (`rustfmt`).
- Clippy on save.
- Show inlay hints for types and parameter names.

VS Code: install `rust-analyzer`. JetBrains: use the IntelliJ Rust plugin.

## Cross-compilation

The server and SDK build in CI for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`, and `x86_64-pc-windows-msvc`. All four are first-class release targets.

```bash
rustup target add aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu
```

## Git workflow

- One branch per change. Branches off `main`.
- PRs require: passing CI, one approving review, no merge conflicts.
- Squash on merge unless the commits are individually meaningful.
- Commit messages follow Conventional Commits (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`).

## Versioning

- Workspace crates share a version, bumped at every release.
- Releases tagged as `vMAJOR.MINOR.PATCH`.
- Pre-1.0: minor bumps may break (anything is on the table).
- Post-1.0: semver applied to the SDK and wire protocol; engine internals are still in flux.

## Logging during development

```bash
RUST_LOG=prism=debug,prism_wal=trace ./target/debug/prismd run ...
```

Per-module log levels via the `tracing` crate. See `operations/observability.md` for details.

## Common dev workflows

### Iterate on a single crate

```bash
cd crates/prism-wal
cargo watch -x check -x 'test -- --nocapture'
```

### Quick sanity check after a refactor

```bash
cargo check
```

This type-checks the workspace in seconds without producing artifacts. Catches the most common errors.

### Reproduce a failing test

```bash
PROPTEST_CASES=100000 cargo test -p prism-mvcc visibility_property
```

Property tests run with a default case count; bump it to reproduce or to gain confidence.

## References

- `Cargo.toml` workspace manifest is the source of truth for crates and dependencies.
- `operations/testing-strategy.md` - what to write tests for.
- `operations/benchmarking.md` - how to measure.
- `project/engineering-standards.md` - style and review conventions.
