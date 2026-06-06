//! `prism-buffer` — the buffer pool.
//!
//! The in-memory cache of pages. Owns a fixed pool of frames; loads pages on
//! demand, pins them during use, and evicts under pressure via clock sweep.
//! Crucially, it enforces the **WAL invariant**: a dirty page never reaches
//! disk until the WAL is durable through that page's `page_lsn`. See
//! `docs/components/buffer-pool.md` and ADR 0007.
//!
//! Built on `prism-storage` (the disk) and `prism-wal` (durability).

pub mod error;
mod pool;

pub use error::{BufferError, Result};
pub use pool::{BufferPool, Config, PageReadGuard, PageWriteGuard};
