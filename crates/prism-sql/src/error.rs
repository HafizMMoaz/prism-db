//! The `prism-sql` error type.

use thiserror::Error;

/// Convenience alias for results in this crate.
pub type Result<T> = std::result::Result<T, SqlError>;

/// Errors produced by the SQL engine.
#[derive(Debug, Error)]
pub enum SqlError {
    /// An error from the transactional core (MVCC, locks, storage).
    #[error(transparent)]
    Core(#[from] prism_core::CoreError),

    /// An error from an index access method (the primary-key B+tree).
    #[error(transparent)]
    Index(#[from] prism_index::IndexError),

    /// A constraint was violated (e.g. a duplicate primary key).
    #[error("constraint violation: {0}")]
    Constraint(String),

    /// The SQL string could not be parsed.
    #[error("parse error: {0}")]
    Parse(String),

    /// A construct that parsed but isn't supported in this subset.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// A referenced table does not exist.
    #[error("no such table: {0}")]
    NoSuchTable(String),

    /// A table already exists.
    #[error("table already exists: {0}")]
    TableExists(String),

    /// A referenced column does not exist.
    #[error("no such column: {0}")]
    NoSuchColumn(String),

    /// A value's type does not match the column (or an expression is ill-typed).
    #[error("type error: {0}")]
    Type(String),

    /// A row stored on disk could not be decoded against the schema.
    #[error("corrupt row: {0}")]
    Corrupt(String),
}
