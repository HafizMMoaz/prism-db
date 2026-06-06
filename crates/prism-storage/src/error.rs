//! The `prism-storage` error type. See the error catalog in
//! `docs/components/disk-manager.md`.

use thiserror::Error;

/// Convenience alias for results in this crate.
pub type Result<T> = std::result::Result<T, StorageError>;

/// Errors produced by the storage layer.
///
/// The disk manager does not retry; I/O errors propagate to the caller, which
/// decides whether to retry, abort, or panic.
#[derive(Debug, Error)]
pub enum StorageError {
    /// An OS-level I/O error. Generally fatal.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A read returned fewer than `PAGE_SIZE` bytes. The page may be partially
    /// written (a crash mid-write); higher layers repair it via WAL redo.
    #[error("short read on page {page}: got {got} of {expected} bytes")]
    ShortRead {
        /// The page being read.
        page: u64,
        /// Bytes actually read.
        got: usize,
        /// Bytes expected (`PAGE_SIZE`).
        expected: usize,
    },

    /// A write persisted fewer than `PAGE_SIZE` bytes. The on-disk page is now
    /// in an indeterminate state.
    #[error("short write on page {page}: wrote {got} of {expected} bytes")]
    ShortWrite {
        /// The page being written.
        page: u64,
        /// Bytes actually written.
        got: usize,
        /// Bytes expected (`PAGE_SIZE`).
        expected: usize,
    },

    /// The database header's magic or version did not match. Refuse to open.
    #[error("incompatible database: {0}")]
    IncompatibleDatabase(String),

    /// Another process holds the file lock.
    #[error("database file is locked by another process")]
    LockedByOtherProcess,

    /// A page or header failed its checksum.
    #[error("checksum mismatch on page {0}")]
    ChecksumMismatch(u64),

    /// A record is too large to fit in a page.
    #[error("record too large: {size} bytes (max {max})")]
    RecordTooLarge {
        /// The attempted record size.
        size: usize,
        /// The maximum permitted size.
        max: usize,
    },
}
