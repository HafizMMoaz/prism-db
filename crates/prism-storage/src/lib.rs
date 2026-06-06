//! `prism-storage` — the storage foundation.
//!
//! Owns the heap file, page read/write primitives, `fsync` semantics, the
//! slotted-page layout, page checksumming, and platform I/O abstraction.
//! No upward dependencies. See `docs/components/disk-manager.md` and
//! `docs/specs/page-format.md`.
//!
//! The crate is organized as:
//! - [`page`] — the on-disk page header and slotted-page operations.
//! - [`db_header`] — page 0, the database header.
//! - [`checksum`] — CRC32 helpers for page and header integrity.
//! - [`disk`] — the [`DiskManager`] and the cross-platform [`IoBackend`] trait.
//! - [`error`] — the crate error type.

pub mod checksum;
pub mod db_header;
pub mod disk;
pub mod error;
pub mod page;

pub use db_header::DbHeader;
pub use disk::{DiskManager, IoBackend, StdFileBackend};
pub use error::{Result, StorageError};
pub use page::{PageType, SlottedPage, SlottedPageRef};

/// Page size in bytes. Compile-time constant; not user-configurable in v1.
pub const PAGE_SIZE: usize = 8192;

/// A logical page identifier: an index into the heap file.
///
/// A `u64` whose upper 16 bits are always zero in v1 (reserved for a future
/// tablespace id), matching the 48-bit page field of a `RecordId`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct PageId(pub u64);

impl PageId {
    /// The byte offset of this page within the heap file.
    #[inline]
    pub const fn byte_offset(self) -> u64 {
        self.0 * PAGE_SIZE as u64
    }

    /// The raw `u64` value.
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for PageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "page#{}", self.0)
    }
}

/// A slot identifier within a page. Slot ids are stable for the life of a record.
pub type SlotId = u16;
