//! CRC32 helpers for page and database-header integrity.
//!
//! Per `docs/specs/page-format.md`, a page's checksum covers bytes
//! `[16, PAGE_SIZE)` — deliberately excluding the `page_lsn` (bytes `0..8`) and
//! the checksum field itself (bytes `8..10`). This lets the WAL update a page's
//! LSN in place under a write latch without recomputing the body checksum.

use crate::PAGE_SIZE;

/// The offset at which the checksummed page body begins.
pub const CHECKSUM_BODY_START: usize = 16;

/// The low 16 bits of the CRC32 over the page body (`[16, PAGE_SIZE)`).
///
/// Sixteen bits is intentionally compact; the WAL's own checksums provide
/// defense in depth (see the page-format spec).
pub fn page_checksum(page: &[u8; PAGE_SIZE]) -> u16 {
    (crc32(&page[CHECKSUM_BODY_START..]) & 0xFFFF) as u16
}

/// A full CRC32 over an arbitrary byte slice (used by the database header).
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(bytes);
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_ignores_lsn_and_checksum_fields() {
        let mut page = [0u8; PAGE_SIZE];
        let base = page_checksum(&page);
        // Mutating the page_lsn (0..8) must not change the body checksum.
        page[0..8].copy_from_slice(&0xDEAD_BEEF_u64.to_le_bytes());
        // Mutating the checksum field (8..10) must not change it either.
        page[8..10].copy_from_slice(&0xABCD_u16.to_le_bytes());
        assert_eq!(base, page_checksum(&page));
    }

    #[test]
    fn checksum_detects_body_change() {
        let mut page = [0u8; PAGE_SIZE];
        let base = page_checksum(&page);
        page[CHECKSUM_BODY_START] ^= 0xFF;
        assert_ne!(base, page_checksum(&page));
    }
}
