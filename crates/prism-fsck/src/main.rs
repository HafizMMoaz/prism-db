//! `prism-fsck` — offline integrity checker.
//!
//! Validates page checksums and WAL integrity; reports orphaned records, broken
//! version chains, and index/heap inconsistencies. Reads formats only, never
//! live state. See `docs/architecture/module-layout.md`.

fn main() {
    eprintln!("prism-fsck: not yet implemented (Phase 5 / hardening).");
    std::process::exit(1);
}
