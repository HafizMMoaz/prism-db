//! The `prism-core` error type.

use thiserror::Error;

use crate::TxnId;

/// Convenience alias for results in this crate.
pub type Result<T> = std::result::Result<T, CoreError>;

/// Errors produced by the transactional core.
#[derive(Debug, Error)]
pub enum CoreError {
    /// An error from the write-ahead log.
    #[error("WAL error: {0}")]
    Wal(#[from] prism_wal::WalError),

    /// An error from the buffer pool.
    #[error("buffer error: {0}")]
    Buffer(#[from] prism_buffer::BufferError),

    /// An error from the storage layer.
    #[error("storage error: {0}")]
    Storage(#[from] prism_storage::StorageError),

    /// A write-write conflict under snapshot isolation: another transaction
    /// committed a change to this record after our snapshot began. Retry.
    #[error("serialization failure (write-write conflict)")]
    SerializationFailure,

    /// An operation referenced a transaction that is not active.
    #[error("transaction {0} is not active")]
    TxnNotActive(TxnId),
}
