//! `prism-wal` — the write-ahead log.
//!
//! Append-only log of page mutations: LSN allocation, group-commit flush,
//! segment rotation, and the replay iterator used by recovery. Built on
//! `prism-storage`. See `docs/components/wal.md`,
//! `docs/specs/wal-record-format.md`, and ADR 0003.
//!
//! Modules:
//! - [`record`] — the [`LogRecord`] enum and frame encode/decode (with CRC).
//! - [`segment`] — segment headers and file naming.
//! - [`wal`] — the [`Wal`] type: open/append/flush_through/replay.
//! - [`error`] — the crate error type.

pub mod error;
pub mod record;
pub mod segment;
pub mod wal;

pub use error::{Result, WalError};
pub use record::{LogRecord, RecordPayload};
pub use wal::{Config, SyncMode, Wal};

/// A log sequence number: the durable, monotonic identifier of a WAL record.
///
/// Encodes its own on-disk location: `lsn = (segment_id << 32) | offset`, where
/// `offset` is the byte offset of the record within its segment.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Lsn(pub u64);

impl Lsn {
    /// The zero LSN — "nothing is durable yet" and the sentinel for an empty
    /// record header slot. No real record is ever assigned LSN 0.
    pub const ZERO: Lsn = Lsn(0);

    /// Build an LSN from a segment id and an in-segment byte offset.
    #[inline]
    pub const fn from_parts(segment_id: u32, offset: u32) -> Lsn {
        Lsn(((segment_id as u64) << 32) | offset as u64)
    }

    /// The segment id encoded in this LSN.
    #[inline]
    pub const fn segment_id(self) -> u32 {
        (self.0 >> 32) as u32
    }

    /// The in-segment byte offset encoded in this LSN.
    #[inline]
    pub const fn offset(self) -> u32 {
        (self.0 & 0xFFFF_FFFF) as u32
    }

    /// The raw `u64` value.
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Lsn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lsn:{}/{}", self.segment_id(), self.offset())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsn_parts_roundtrip() {
        let l = Lsn::from_parts(7, 4096);
        assert_eq!(l.segment_id(), 7);
        assert_eq!(l.offset(), 4096);
        assert_eq!(l, Lsn(l.as_u64()));
    }

    #[test]
    fn lsn_orders_by_segment_then_offset() {
        assert!(Lsn::from_parts(0, 5000) < Lsn::from_parts(1, 64));
        assert!(Lsn::from_parts(2, 64) < Lsn::from_parts(2, 65));
    }
}
