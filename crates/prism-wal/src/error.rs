//! The `prism-wal` error type.

use thiserror::Error;

/// Convenience alias for results in this crate.
pub type Result<T> = std::result::Result<T, WalError>;

/// Errors produced by the write-ahead log.
#[derive(Debug, Error)]
pub enum WalError {
    /// An OS-level I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A record is larger than a segment can ever hold.
    #[error("WAL record too large: {size} bytes (a segment holds at most {max})")]
    RecordTooLarge {
        /// The attempted on-disk frame size.
        size: usize,
        /// The largest frame a segment can hold.
        max: usize,
    },

    /// A record header carried an unknown type discriminator.
    #[error("unknown WAL record type: 0x{0:02x}")]
    UnknownRecordType(u8),

    /// A record or segment header was malformed.
    #[error("malformed WAL data: {0}")]
    Decode(String),

    /// A record's CRC did not match. During replay this marks the end of the
    /// valid log (a torn write); every byte from here on is discarded.
    #[error("CRC mismatch (torn write / end of log)")]
    CrcMismatch,

    /// A segment header had the wrong magic bytes.
    #[error("bad WAL segment magic")]
    BadMagic,

    /// A segment was written by an incompatible format version.
    #[error("incompatible WAL format version: {0}")]
    IncompatibleVersion(u32),
}
