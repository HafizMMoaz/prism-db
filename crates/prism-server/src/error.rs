//! The server error type and its mapping to a protocol error trailer.

use prism_core::CoreError;
use prism_doc::DocError;
use prism_kv::KvError;
use prism_protocol::ErrorInfo;
use prism_sql::SqlError;
use thiserror::Error;

/// Convenience alias.
pub type Result<T> = std::result::Result<T, ServerError>;

/// An error handling a request. Carries the originating engine error so the
/// dispatcher can map it to a wire [`ErrorInfo`] with an appropriate code.
#[derive(Debug, Error)]
pub enum ServerError {
    /// A relational-engine error.
    #[error(transparent)]
    Sql(#[from] SqlError),
    /// A document-engine error.
    #[error(transparent)]
    Doc(#[from] DocError),
    /// A key-value-engine error.
    #[error(transparent)]
    Kv(#[from] KvError),
    /// A transactional-core error (MVCC, locks, storage, recovery).
    #[error(transparent)]
    Core(#[from] CoreError),
    /// The session received a request it cannot serve in its current state
    /// (e.g. `Commit` with no open transaction).
    #[error("protocol state error: {0}")]
    State(String),
    /// A request used a feature this build does not yet support.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The authenticated user lacks the privilege the request requires.
    #[error("permission denied: {0}")]
    Unauthorized(String),
    /// Persistent state (e.g. a catalog record) could not be decoded.
    #[error("corrupt: {0}")]
    Corrupt(String),
}

impl From<prism_protocol::ProtocolError> for ServerError {
    fn from(e: prism_protocol::ProtocolError) -> Self {
        ServerError::Corrupt(e.to_string())
    }
}

// Storage-layer errors only surface while opening the database; route them
// through CoreError so they share the "Core" category.
impl From<prism_storage::StorageError> for ServerError {
    fn from(e: prism_storage::StorageError) -> Self {
        ServerError::Core(e.into())
    }
}
impl From<prism_wal::WalError> for ServerError {
    fn from(e: prism_wal::WalError) -> Self {
        ServerError::Core(e.into())
    }
}
impl From<prism_buffer::BufferError> for ServerError {
    fn from(e: prism_buffer::BufferError) -> Self {
        ServerError::Core(e.into())
    }
}

impl ServerError {
    /// The wire error code (from the ranges in `docs/specs/wire-protocol.md`).
    fn error_code(&self) -> u32 {
        match self {
            // 0x0400–0x04FF query errors (syntax, plan, type).
            ServerError::Sql(_) | ServerError::Doc(_) | ServerError::Kv(_) => 0x0400,
            // 0x0200–0x02FF transaction errors (serialization, deadlock, …).
            ServerError::Core(_) => 0x0200,
            // 0x0001–0x00FF protocol errors.
            ServerError::State(_) => 0x0001,
            // 0x0100–0x01FF authentication / authorization.
            ServerError::Unauthorized(_) => 0x0101,
            // 0xFF00–0xFFFF internal / unexpected (incl. not-yet-implemented).
            ServerError::Unsupported(_) => 0xFF01,
            ServerError::Corrupt(_) => 0xFF02,
        }
    }

    /// Build the wire error trailer for this error.
    pub fn to_error_info(&self) -> ErrorInfo {
        ErrorInfo {
            error_code: self.error_code(),
            message: self.to_string(),
            sqlstate: *b"XX000",
            detail: String::new(),
            position: 0,
        }
    }
}
