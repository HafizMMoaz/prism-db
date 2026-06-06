//! The `prism-buffer` error type.

use prism_storage::StorageError;
use prism_wal::WalError;
use thiserror::Error;

/// Convenience alias for results in this crate.
pub type Result<T> = std::result::Result<T, BufferError>;

/// Errors produced by the buffer pool.
#[derive(Debug, Error)]
pub enum BufferError {
    /// An error from the underlying disk manager.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// An error from the underlying write-ahead log.
    #[error("WAL error: {0}")]
    Wal(#[from] WalError),

    /// No frame could be evicted: every frame is pinned. The caller should
    /// treat this as an out-of-memory condition.
    #[error("buffer pool exhausted: all {frames} frames are pinned")]
    Exhausted {
        /// The number of frames in the pool.
        frames: usize,
    },
}
