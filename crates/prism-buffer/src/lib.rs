//! `prism-buffer` — the buffer pool.
//!
//! Fixed-size frame array, page table (`PageId -> FrameId`), pin/unpin,
//! clock-sweep eviction, dirty tracking, and the background page cleaner.
//! Enforces the WAL invariant: a dirty page is never flushed before the log
//! record describing its modification is durable. See
//! `docs/components/buffer-pool.md` and `docs/adr/0007-clock-sweep-buffer-pool.md`.
//!
//! Status: skeleton (Phase 1 / M1 not yet started).
