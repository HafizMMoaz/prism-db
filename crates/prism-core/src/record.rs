//! The MVCC record (tuple) header and the [`RecordId`] type.
//!
//! Byte layout is normative; see "Record header" in `docs/specs/record-format.md`.
//! The header is the same across all three access methods; only the payload that
//! follows it differs.

use prism_storage::{PageId, SlotId};

use crate::{NO_TXN, TxnId};

/// Size of the MVCC record header, in bytes.
pub const RECORD_HEADER_SIZE: usize = 24;

// Flags (byte offset 22, u16).
/// `prev_version` is valid (otherwise NIL).
pub const FLAG_HAS_PREV: u16 = 1 << 0;
/// The payload is a forwarding `RecordId`, not a normal record.
pub const FLAG_FORWARDED: u16 = 1 << 1;
/// The record is a tombstone (deleted; payload may be meaningless).
pub const FLAG_TOMBSTONE: u16 = 1 << 2;
/// The row is currently write-locked (advisory).
pub const FLAG_LOCKED: u16 = 1 << 3;

/// A record identifier: a page plus a slot within it.
///
/// Encoded into 6 bytes for `prev_version` chains (4-byte page + 2-byte slot),
/// which caps a v1 database at `2^32` pages (32 TiB) — a documented limit.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RecordId {
    /// The page holding the record.
    pub page: PageId,
    /// The slot within the page.
    pub slot: SlotId,
}

impl RecordId {
    /// The 6-byte NIL sentinel (`0xFFFF_FFFF_FFFF`).
    const NIL6: [u8; 6] = [0xFF; 6];

    /// Create a record id.
    pub fn new(page: PageId, slot: SlotId) -> Self {
        Self { page, slot }
    }

    /// Encode an optional record id into the 6-byte `prev_version` form.
    pub fn encode6(this: Option<RecordId>) -> [u8; 6] {
        match this {
            None => Self::NIL6,
            Some(rid) => {
                let page = rid.page.as_u64() as u32; // low 32 bits
                let p = page.to_le_bytes();
                let s = rid.slot.to_le_bytes();
                [p[0], p[1], p[2], p[3], s[0], s[1]]
            }
        }
    }

    /// Decode a 6-byte `prev_version` field, returning `None` for NIL.
    pub fn decode6(bytes: [u8; 6]) -> Option<RecordId> {
        if bytes == Self::NIL6 {
            return None;
        }
        let page = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let slot = u16::from_le_bytes([bytes[4], bytes[5]]);
        Some(RecordId {
            page: PageId(page as u64),
            slot,
        })
    }
}

impl std::fmt::Display for RecordId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rid({},{})", self.page.as_u64(), self.slot)
    }
}

/// The 24-byte MVCC header that prefixes every record's payload.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RecordHeader {
    /// The transaction that created this version.
    pub xmin: TxnId,
    /// The transaction that deleted/superseded this version (0 = current).
    pub xmax: TxnId,
    /// The previous (older) version in the chain, or `None`.
    pub prev_version: Option<RecordId>,
    /// Flag bits. `FLAG_HAS_PREV` is derived from `prev_version` on encode.
    pub flags: u16,
}

impl RecordHeader {
    /// A header for a freshly inserted version by `xmin` (not deleted, no chain).
    pub fn new_insert(xmin: TxnId) -> Self {
        Self {
            xmin,
            xmax: NO_TXN,
            prev_version: None,
            flags: 0,
        }
    }

    /// Whether this version has been marked deleted/superseded.
    pub fn is_deleted(&self) -> bool {
        self.xmax != NO_TXN
    }

    /// Encode the header into its fixed 24-byte form.
    pub fn encode(&self) -> [u8; RECORD_HEADER_SIZE] {
        let mut buf = [0u8; RECORD_HEADER_SIZE];
        buf[0..8].copy_from_slice(&self.xmin.to_le_bytes());
        buf[8..16].copy_from_slice(&self.xmax.to_le_bytes());
        buf[16..22].copy_from_slice(&RecordId::encode6(self.prev_version));
        let mut flags = self.flags & !FLAG_HAS_PREV;
        if self.prev_version.is_some() {
            flags |= FLAG_HAS_PREV;
        }
        buf[22..24].copy_from_slice(&flags.to_le_bytes());
        buf
    }

    /// Decode a header from the first 24 bytes of `bytes`.
    ///
    /// Returns `None` if `bytes` is shorter than the header.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_HEADER_SIZE {
            return None;
        }
        let xmin = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        let xmax = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
        let prev6: [u8; 6] = bytes[16..22].try_into().ok()?;
        let flags = u16::from_le_bytes(bytes[22..24].try_into().ok()?);
        Some(Self {
            xmin,
            xmax,
            prev_version: RecordId::decode6(prev6),
            flags,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_id_encode_decode() {
        assert_eq!(RecordId::decode6(RecordId::encode6(None)), None);
        let rid = RecordId::new(PageId(0x0102_0304), 0x0506);
        assert_eq!(RecordId::decode6(RecordId::encode6(Some(rid))), Some(rid));
        // NIL must round-trip to None.
        assert_eq!(RecordId::encode6(None), [0xFF; 6]);
    }

    #[test]
    fn header_roundtrip_insert() {
        let h = RecordHeader::new_insert(42);
        let back = RecordHeader::decode(&h.encode()).unwrap();
        assert_eq!(h, back);
        assert!(!back.is_deleted());
        // No prev => HAS_PREV not set.
        assert_eq!(back.flags & FLAG_HAS_PREV, 0);
    }

    #[test]
    fn header_roundtrip_with_chain_and_delete() {
        let h = RecordHeader {
            xmin: 10,
            xmax: 30,
            prev_version: Some(RecordId::new(PageId(7), 3)),
            flags: FLAG_TOMBSTONE,
        };
        let encoded = h.encode();
        let back = RecordHeader::decode(&encoded).unwrap();
        assert_eq!(back.xmin, 10);
        assert_eq!(back.xmax, 30);
        assert_eq!(back.prev_version, Some(RecordId::new(PageId(7), 3)));
        assert!(back.is_deleted());
        // HAS_PREV is set automatically; TOMBSTONE preserved.
        assert_ne!(back.flags & FLAG_HAS_PREV, 0);
        assert_ne!(back.flags & FLAG_TOMBSTONE, 0);
    }

    #[test]
    fn decode_rejects_short_input() {
        assert!(RecordHeader::decode(&[0u8; 10]).is_none());
    }
}
