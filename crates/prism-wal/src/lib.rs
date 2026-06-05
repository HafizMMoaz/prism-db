//! `prism-wal` — the write-ahead log.
//!
//! Append-only log of page mutations, LSN allocation, group commit, segment
//! rotation, and the replay iterator used by recovery. Built on `prism-storage`.
//! See `docs/components/wal.md` and `docs/adr/0003-physiological-wal-aries.md`.
//!
//! Status: skeleton (Phase 1 / M1 not yet started).
