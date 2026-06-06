//! The on-disk page header and slotted-page operations.
//!
//! Byte layout is normative; see `docs/specs/page-format.md`. All multi-byte
//! integers are little-endian.
//!
//! ```text
//! ┌──────────────────────────────────────────────┐
//! │ PageHeader (32 bytes)                        │
//! ├──────────────────────────────────────────────┤
//! │ Slot 0, Slot 1, ...   (4 bytes each)         │  grows down from 32
//! ├──────────────────────────────────────────────┤
//! │                Free space                    │
//! ├──────────────────────────────────────────────┤
//! │ Record bytes (newest at low offset)          │  grows up from PAGE_SIZE
//! └──────────────────────────────────────────────┘
//! ```

use crate::{PAGE_SIZE, SlotId};

/// Size of the fixed page header, in bytes.
pub const PAGE_HEADER_SIZE: usize = 32;
/// Size of a single slot entry, in bytes.
pub const SLOT_SIZE: usize = 4;
/// The offset at which the slot array begins (immediately after the header).
pub const SLOT_ARRAY_START: usize = PAGE_HEADER_SIZE;

// Page header field offsets.
const OFF_PAGE_LSN: usize = 0;
const OFF_CHECKSUM: usize = 8;
const OFF_PAGE_TYPE: usize = 10;
const OFF_FREE_START: usize = 14; // free_space_offset (low end of free region)
const OFF_FREE_END: usize = 16; // free_space_end (high end of free region)
const OFF_SLOT_COUNT: usize = 18;

// Within a slot: u16 record_offset, then u16 record_length (high bit = forward).
const SLOT_OFF_OFFSET: usize = 0;
const SLOT_OFF_LENGTH: usize = 2;

/// High bit of a slot's length field: the record payload is a forwarding RID.
pub const SLOT_FORWARD_FLAG: u16 = 0x8000;
/// Mask for the actual record length within a slot's length field.
pub const SLOT_LENGTH_MASK: u16 = 0x7FFF;

/// The maximum length of a single record placed in a page, in bytes.
///
/// Bounded both by the 15-bit length field and by the available page body.
pub const MAX_RECORD_LEN: usize = {
    let by_field = SLOT_LENGTH_MASK as usize;
    let by_page = PAGE_SIZE - PAGE_HEADER_SIZE - SLOT_SIZE;
    if by_field < by_page {
        by_field
    } else {
        by_page
    }
};

/// The type of a page, stored at byte offset 10 of the header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PageType {
    /// A heap page holding records.
    Heap = 1,
    /// A B+tree internal (non-leaf) page.
    BTreeInternal = 2,
    /// A B+tree leaf page.
    BTreeLeaf = 3,
    /// A hash bucket page.
    HashBucket = 4,
    /// A hash overflow page.
    HashOverflow = 5,
    /// An allocated-but-unused page.
    Free = 6,
}

impl PageType {
    /// Decode a page type from its on-disk byte, if valid.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Heap),
            2 => Some(Self::BTreeInternal),
            3 => Some(Self::BTreeLeaf),
            4 => Some(Self::HashBucket),
            5 => Some(Self::HashOverflow),
            6 => Some(Self::Free),
            _ => None,
        }
    }
}

#[inline]
fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

#[inline]
fn wr_u16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

#[inline]
fn rd_u64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

// Shared header/slot reads over a raw page buffer, used by both the read-only
// and mutable views below.
#[inline]
fn hdr_page_lsn(b: &[u8; PAGE_SIZE]) -> u64 {
    rd_u64(b, OFF_PAGE_LSN)
}
#[inline]
fn hdr_page_type(b: &[u8; PAGE_SIZE]) -> Option<PageType> {
    PageType::from_u8(b[OFF_PAGE_TYPE])
}
#[inline]
fn hdr_slot_count(b: &[u8; PAGE_SIZE]) -> u16 {
    rd_u16(b, OFF_SLOT_COUNT)
}
#[inline]
fn hdr_free_start(b: &[u8; PAGE_SIZE]) -> u16 {
    rd_u16(b, OFF_FREE_START)
}
#[inline]
fn hdr_free_end(b: &[u8; PAGE_SIZE]) -> u16 {
    rd_u16(b, OFF_FREE_END)
}
#[inline]
fn slot_base(i: usize) -> usize {
    SLOT_ARRAY_START + i * SLOT_SIZE
}
#[inline]
fn slot_offset(b: &[u8; PAGE_SIZE], i: usize) -> u16 {
    rd_u16(b, slot_base(i) + SLOT_OFF_OFFSET)
}
#[inline]
fn slot_length_field(b: &[u8; PAGE_SIZE], i: usize) -> u16 {
    rd_u16(b, slot_base(i) + SLOT_OFF_LENGTH)
}

fn record_at(b: &[u8; PAGE_SIZE], id: SlotId) -> Option<&[u8]> {
    let i = id as usize;
    if i >= hdr_slot_count(b) as usize {
        return None;
    }
    let off = slot_offset(b, i) as usize;
    if off == 0 {
        return None; // empty slot
    }
    let len = (slot_length_field(b, i) & SLOT_LENGTH_MASK) as usize;
    b.get(off..off + len)
}

/// A read-only view over a page buffer.
pub struct SlottedPageRef<'a> {
    buf: &'a [u8; PAGE_SIZE],
}

impl<'a> SlottedPageRef<'a> {
    /// Wrap a page buffer for read-only access.
    pub fn new(buf: &'a [u8; PAGE_SIZE]) -> Self {
        Self { buf }
    }

    /// The LSN of the last log record that modified this page.
    pub fn page_lsn(&self) -> u64 {
        hdr_page_lsn(self.buf)
    }

    /// The page type, if the type byte is valid.
    pub fn page_type(&self) -> Option<PageType> {
        hdr_page_type(self.buf)
    }

    /// The number of slots (including empty ones) in the slot array.
    pub fn slot_count(&self) -> u16 {
        hdr_slot_count(self.buf)
    }

    /// The record bytes for `id`, or `None` if the slot is empty or invalid.
    pub fn get(&self, id: SlotId) -> Option<&[u8]> {
        record_at(self.buf, id)
    }

    /// Whether slot `id` is a forwarding pointer (its payload is a `RecordId`).
    pub fn is_forwarding(&self, id: SlotId) -> bool {
        let i = id as usize;
        i < self.slot_count() as usize
            && slot_offset(self.buf, i) != 0
            && slot_length_field(self.buf, i) & SLOT_FORWARD_FLAG != 0
    }

    /// Whether the stored body checksum matches the page contents.
    pub fn verify_checksum(&self) -> bool {
        rd_u16(self.buf, OFF_CHECKSUM) == crate::checksum::page_checksum(self.buf)
    }
}

/// A mutable view over a page buffer, providing slotted-page operations.
pub struct SlottedPage<'a> {
    buf: &'a mut [u8; PAGE_SIZE],
}

impl<'a> SlottedPage<'a> {
    /// Wrap an existing page buffer (does not modify it).
    pub fn new(buf: &'a mut [u8; PAGE_SIZE]) -> Self {
        Self { buf }
    }

    /// Initialize `buf` as an empty page of the given type.
    ///
    /// Zeroes the buffer, then sets the header: empty slot array, free region
    /// spanning `[32, PAGE_SIZE)`, and `page_lsn = 0`.
    pub fn init(buf: &'a mut [u8; PAGE_SIZE], page_type: PageType) -> Self {
        buf.fill(0);
        let mut page = Self { buf };
        page.buf[OFF_PAGE_TYPE] = page_type as u8;
        page.set_free_start(SLOT_ARRAY_START as u16);
        page.set_free_end(PAGE_SIZE as u16);
        page.set_slot_count(0);
        page
    }

    // ── Header accessors ────────────────────────────────────────────────

    /// The LSN of the last log record that modified this page.
    pub fn page_lsn(&self) -> u64 {
        hdr_page_lsn(self.buf)
    }

    /// Set the page LSN. Safe to call under a write latch without touching the
    /// body checksum (the LSN is outside the checksummed region).
    pub fn set_page_lsn(&mut self, lsn: u64) {
        self.buf[OFF_PAGE_LSN..OFF_PAGE_LSN + 8].copy_from_slice(&lsn.to_le_bytes());
    }

    /// The page type, if the type byte is valid.
    pub fn page_type(&self) -> Option<PageType> {
        hdr_page_type(self.buf)
    }

    /// The number of slots (including empty ones).
    pub fn slot_count(&self) -> u16 {
        hdr_slot_count(self.buf)
    }

    /// The size of the contiguous free region, in bytes.
    pub fn free_space(&self) -> usize {
        (hdr_free_end(self.buf) - hdr_free_start(self.buf)) as usize
    }

    /// The low end of the free region (the byte just past the slot array).
    pub fn free_start(&self) -> u16 {
        hdr_free_start(self.buf)
    }

    /// The high end of the free region (the start of the record data).
    pub fn free_end(&self) -> u16 {
        hdr_free_end(self.buf)
    }

    fn set_free_start(&mut self, v: u16) {
        wr_u16(self.buf, OFF_FREE_START, v);
    }

    fn set_free_end(&mut self, v: u16) {
        wr_u16(self.buf, OFF_FREE_END, v);
    }

    fn set_slot_count(&mut self, v: u16) {
        wr_u16(self.buf, OFF_SLOT_COUNT, v);
    }

    fn set_slot(&mut self, i: usize, offset: u16, length_field: u16) {
        let base = slot_base(i);
        wr_u16(self.buf, base + SLOT_OFF_OFFSET, offset);
        wr_u16(self.buf, base + SLOT_OFF_LENGTH, length_field);
    }

    // ── Record operations ───────────────────────────────────────────────

    /// The record bytes for `id`, or `None` if the slot is empty or invalid.
    pub fn get(&self, id: SlotId) -> Option<&[u8]> {
        record_at(self.buf, id)
    }

    /// Mutable access to the record bytes for `id`, for in-place edits that do
    /// not change the record's length (e.g. stamping an MVCC `xmax`). Returns
    /// `None` if the slot is empty or invalid.
    pub fn get_mut(&mut self, id: SlotId) -> Option<&mut [u8]> {
        let i = id as usize;
        if i >= self.slot_count() as usize {
            return None;
        }
        let off = slot_offset(self.buf, i) as usize;
        if off == 0 {
            return None;
        }
        let len = (slot_length_field(self.buf, i) & SLOT_LENGTH_MASK) as usize;
        self.buf.get_mut(off..off + len)
    }

    /// Insert a record, returning its slot id, or `None` if it does not fit.
    ///
    /// Reuses an empty slot when one exists (saving the 4-byte slot overhead);
    /// otherwise appends a new slot. Records are placed at the high end of the
    /// page, growing downward.
    pub fn insert(&mut self, record: &[u8]) -> Option<SlotId> {
        let len = record.len();
        if len > MAX_RECORD_LEN {
            return None;
        }

        let count = self.slot_count() as usize;
        let reuse = (0..count).find(|&i| slot_offset(self.buf, i) == 0);

        let required = len + if reuse.is_some() { 0 } else { SLOT_SIZE };
        if self.free_space() < required {
            return None;
        }

        let new_off = hdr_free_end(self.buf) as usize - len;
        self.buf[new_off..new_off + len].copy_from_slice(record);

        let slot_id = match reuse {
            Some(i) => i,
            None => {
                self.set_slot_count((count + 1) as u16);
                self.set_free_start((SLOT_ARRAY_START + (count + 1) * SLOT_SIZE) as u16);
                count
            }
        };
        self.set_slot(slot_id, new_off as u16, len as u16);
        self.set_free_end(new_off as u16);
        Some(slot_id as SlotId)
    }

    /// Mark slot `id` empty. The record bytes remain until the next compaction.
    /// Returns whether the slot existed and was occupied.
    pub fn delete(&mut self, id: SlotId) -> bool {
        let i = id as usize;
        if i >= self.slot_count() as usize || slot_offset(self.buf, i) == 0 {
            return false;
        }
        self.set_slot(i, 0, 0);
        true
    }

    /// Compact the page, reclaiming space left by deleted records.
    ///
    /// Live records are rewritten contiguously at the high end of the page and
    /// their slot offsets updated. Slot ids are preserved (empty slots stay
    /// empty), so existing `RecordId`s remain valid.
    pub fn compact(&mut self) {
        let count = self.slot_count() as usize;
        let mut live: Vec<(usize, u16, Vec<u8>)> = Vec::new();
        for i in 0..count {
            let off = slot_offset(self.buf, i) as usize;
            if off == 0 {
                continue;
            }
            let length_field = slot_length_field(self.buf, i);
            let len = (length_field & SLOT_LENGTH_MASK) as usize;
            live.push((i, length_field, self.buf[off..off + len].to_vec()));
        }

        let mut end = PAGE_SIZE;
        for (i, length_field, bytes) in &live {
            let new_off = end - bytes.len();
            self.buf[new_off..end].copy_from_slice(bytes);
            self.set_slot(*i, new_off as u16, *length_field);
            end = new_off;
        }
        self.set_free_end(end as u16);
    }

    /// Recompute and store the page body checksum. Call after any body change,
    /// before writing the page to disk.
    pub fn update_checksum(&mut self) {
        let c = crate::checksum::page_checksum(self.buf);
        wr_u16(self.buf, OFF_CHECKSUM, c);
    }

    /// Whether the stored body checksum matches the page contents.
    pub fn verify_checksum(&self) -> bool {
        rd_u16(self.buf, OFF_CHECKSUM) == crate::checksum::page_checksum(self.buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::collection::vec as pvec;
    use proptest::prelude::*;
    use std::collections::HashMap;

    fn fresh() -> Box<[u8; PAGE_SIZE]> {
        let mut buf = Box::new([0u8; PAGE_SIZE]);
        SlottedPage::init(&mut buf, PageType::Heap);
        buf
    }

    #[test]
    fn init_sets_header() {
        let buf = fresh();
        let p = SlottedPageRef::new(&buf);
        assert_eq!(p.page_type(), Some(PageType::Heap));
        assert_eq!(p.slot_count(), 0);
        assert_eq!(p.page_lsn(), 0);
        let mut buf2 = buf.clone();
        let page = SlottedPage::new(&mut buf2);
        assert_eq!(page.free_space(), PAGE_SIZE - PAGE_HEADER_SIZE);
    }

    #[test]
    fn insert_get_roundtrip() {
        let mut buf = fresh();
        let mut page = SlottedPage::new(&mut buf);
        let a = page.insert(b"hello").unwrap();
        let b = page.insert(b"world!!").unwrap();
        assert_eq!(page.get(a), Some(&b"hello"[..]));
        assert_eq!(page.get(b), Some(&b"world!!"[..]));
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(page.slot_count(), 2);
    }

    #[test]
    fn get_mut_edits_in_place() {
        let mut buf = fresh();
        let mut page = SlottedPage::new(&mut buf);
        let a = page.insert(b"abcd").unwrap();
        page.get_mut(a).unwrap().copy_from_slice(b"WXYZ");
        assert_eq!(page.get(a), Some(&b"WXYZ"[..]));
        // Empty / out-of-range slots yield None.
        assert!(page.get_mut(99).is_none());
    }

    #[test]
    fn delete_frees_slot_and_get_returns_none() {
        let mut buf = fresh();
        let mut page = SlottedPage::new(&mut buf);
        let a = page.insert(b"alpha").unwrap();
        assert!(page.delete(a));
        assert_eq!(page.get(a), None);
        assert!(!page.delete(a)); // already empty
        // A subsequent insert reuses the empty slot.
        let b = page.insert(b"beta").unwrap();
        assert_eq!(b, a);
        assert_eq!(page.slot_count(), 1);
    }

    #[test]
    fn insert_fails_when_full() {
        let mut buf = fresh();
        let mut page = SlottedPage::new(&mut buf);
        let big = vec![0xABu8; MAX_RECORD_LEN];
        assert!(page.insert(&big).is_some());
        assert!(page.insert(b"x").is_none());
        assert!(page.insert(&vec![0u8; MAX_RECORD_LEN + 1]).is_none());
    }

    #[test]
    fn compaction_reclaims_deleted_space() {
        let mut buf = fresh();
        let mut page = SlottedPage::new(&mut buf);
        // Fill with several records, delete the early ones, then compact.
        let rec = vec![0x5Au8; 1000];
        let mut ids = Vec::new();
        while let Some(id) = page.insert(&rec) {
            ids.push(id);
        }
        let kept = *ids.last().unwrap();
        for id in &ids[..ids.len() - 1] {
            page.delete(*id);
        }
        let before = page.free_space();
        page.compact();
        assert!(page.free_space() > before);
        // The surviving record is intact and a new insert now fits.
        assert_eq!(page.get(kept), Some(&rec[..]));
        assert!(page.insert(&rec).is_some());
    }

    #[test]
    fn checksum_roundtrip() {
        let mut buf = fresh();
        {
            let mut page = SlottedPage::new(&mut buf);
            page.insert(b"data").unwrap();
            page.update_checksum();
            assert!(page.verify_checksum());
        } // the &mut borrow ends here, before we touch buf directly
        // Corrupt the body; verification must fail.
        buf[PAGE_SIZE - 1] ^= 0xFF;
        assert!(!SlottedPageRef::new(&buf).verify_checksum());
    }

    proptest! {
        /// Model-based test: a sequence of inserts and deletes must keep the
        /// page's invariants and agree with a HashMap oracle on live records.
        #[test]
        fn slotted_matches_model(ops in pvec((any::<bool>(), pvec(any::<u8>(), 0..64)), 0..400)) {
            let mut buf = fresh();
            let mut page = SlottedPage::new(&mut buf);
            let mut model: HashMap<SlotId, Vec<u8>> = HashMap::new();

            for (is_insert, payload) in ops {
                if is_insert {
                    if let Some(id) = page.insert(&payload) {
                        model.insert(id, payload);
                    }
                } else if let Some((&id, _)) = model.iter().next() {
                    prop_assert!(page.delete(id));
                    model.remove(&id);
                }

                // Invariant: free_space_offset tracks the slot array exactly.
                prop_assert_eq!(
                    page.free_start() as usize,
                    SLOT_ARRAY_START + page.slot_count() as usize * SLOT_SIZE
                );
                // Invariant: the free region is non-negative and in-bounds.
                prop_assert!(page.free_start() <= page.free_end());
                prop_assert!(page.free_end() as usize <= PAGE_SIZE);

                // Oracle: every live record reads back exactly.
                for (&id, bytes) in &model {
                    prop_assert_eq!(page.get(id), Some(bytes.as_slice()));
                }
            }

            // Compaction preserves the live set.
            page.compact();
            for (&id, bytes) in &model {
                prop_assert_eq!(page.get(id), Some(bytes.as_slice()));
            }
        }
    }
}
