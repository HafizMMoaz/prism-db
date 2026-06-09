//! The `prism-index` error type.

use thiserror::Error;

/// Convenience alias for results in this crate.
pub type Result<T> = std::result::Result<T, IndexError>;

/// Errors produced by the index access methods.
#[derive(Debug, Error)]
pub enum IndexError {
    /// An error from the buffer pool / storage layer.
    #[error("buffer error: {0}")]
    Buffer(#[from] prism_buffer::BufferError),

    /// An error appending to the write-ahead log.
    #[error("WAL error: {0}")]
    Wal(#[from] prism_wal::WalError),

    /// An index page failed to decode (corruption or version mismatch).
    #[error("index page corrupt: {0}")]
    Corrupt(String),

    /// A single entry is too large to fit in an index node.
    #[error("key/entry too large for an index node")]
    EntryTooLarge,
}
