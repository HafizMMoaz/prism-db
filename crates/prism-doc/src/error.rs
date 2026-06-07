//! The `prism-doc` error type.

use thiserror::Error;

/// Convenience alias for results in this crate.
pub type Result<T> = std::result::Result<T, DocError>;

/// Errors produced by the document engine.
#[derive(Debug, Error)]
pub enum DocError {
    /// An error from the transactional core (MVCC, locks, storage).
    #[error(transparent)]
    Core(#[from] prism_core::CoreError),

    /// A stored document could not be decoded.
    #[error("corrupt document: {0}")]
    Corrupt(String),

    /// A document or field exceeds a size limit.
    #[error("document too large: {0}")]
    TooLarge(String),
}
