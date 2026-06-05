//! `prism-storage` — the storage foundation.
//!
//! Owns the heap file, page read/write primitives, `fsync` semantics, the
//! slotted-page layout, page checksumming, and platform I/O abstraction.
//! No upward dependencies. See `docs/components/disk-manager.md` and
//! `docs/specs/page-format.md`.
//!
//! Status: skeleton (Phase 1 / M1 not yet started).

/// Page size in bytes. Compile-time constant; not user-configurable in v1.
pub const PAGE_SIZE: usize = 8192;
