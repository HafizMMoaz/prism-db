//! WAL log records and their on-disk framing.
//!
//! A frame is a 32-byte record header, a variable body, and a 4-byte CRC32 over
//! header+body. Byte layout is normative; see `docs/specs/wal-record-format.md`.

use prism_storage::{PageId, SlotId};

use crate::Lsn;
use crate::error::{Result, WalError};

/// Size of the fixed record header, in bytes.
pub const RECORD_HEADER_SIZE: usize = 32;
/// Size of the trailing per-record CRC32, in bytes.
pub const RECORD_CRC_SIZE: usize = 4;

// Record type discriminators (see the record-format spec).
const T_INSERT: u8 = 0x01;
const T_UPDATE: u8 = 0x02;
const T_DELETE: u8 = 0x03;
const T_COMMIT: u8 = 0x10;
const T_ABORT: u8 = 0x11;
const T_CLR: u8 = 0x20;
const T_BEGIN_CHECKPOINT: u8 = 0x30;
const T_CHECKPOINT_CONTENTS: u8 = 0x31;
const T_END_CHECKPOINT: u8 = 0x32;
const T_FULL_PAGE_IMAGE: u8 = 0x50;
const T_HEAP_PAGE: u8 = 0x60;

/// A single WAL record: common header fields plus a typed payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord {
    /// The transaction that produced this record (0 for checkpoint records).
    pub txn_id: u64,
    /// The previous record by the same transaction (0 if first). Forms the
    /// backward chain used by undo.
    pub prev_lsn: u64,
    /// The typed payload.
    pub payload: RecordPayload,
}

impl LogRecord {
    /// A record with no transaction context (checkpoint markers, etc.).
    pub fn system(payload: RecordPayload) -> Self {
        Self {
            txn_id: 0,
            prev_lsn: 0,
            payload,
        }
    }

    /// A transactional record.
    pub fn txn(txn_id: u64, prev_lsn: Lsn, payload: RecordPayload) -> Self {
        Self {
            txn_id,
            prev_lsn: prev_lsn.as_u64(),
            payload,
        }
    }
}

/// The typed body of a WAL record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordPayload {
    /// A record was inserted at `slot_id` on `page_id`; `after_image` is the
    /// full record bytes.
    Insert {
        /// Heap/index page modified.
        page_id: PageId,
        /// Slot within the page.
        slot_id: SlotId,
        /// Full post-insert record bytes.
        after_image: Vec<u8>,
    },
    /// A record was updated in place; both images are retained for undo.
    Update {
        /// Page modified.
        page_id: PageId,
        /// Slot within the page.
        slot_id: SlotId,
        /// Record bytes before the update.
        before_image: Vec<u8>,
        /// Record bytes after the update.
        after_image: Vec<u8>,
    },
    /// A record was deleted; `before_image` is retained for undo.
    Delete {
        /// Page modified.
        page_id: PageId,
        /// Slot within the page.
        slot_id: SlotId,
        /// Record bytes before the delete.
        before_image: Vec<u8>,
    },
    /// The transaction committed.
    Commit {
        /// Wall-clock commit time, microseconds since the Unix epoch.
        commit_micros: i64,
        /// Reserved flags.
        flags: u32,
    },
    /// The transaction aborted.
    Abort,
    /// A compensation log record written during undo.
    Clr {
        /// Page modified by the undo.
        page_id: PageId,
        /// Slot within the page.
        slot_id: SlotId,
        /// Bytes written back to the page (the before-image being restored).
        undo_image: Vec<u8>,
        /// Where undo of this transaction resumes next (0 if done).
        undo_next_lsn: u64,
    },
    /// Marks the start of a checkpoint.
    BeginCheckpoint {
        /// Identifier tying the begin/contents/end records together.
        checkpoint_id: u64,
    },
    /// The dirty-page and active-transaction tables captured at checkpoint time.
    CheckpointContents {
        /// `(page_id, rec_lsn)` for each dirty page.
        dirty_pages: Vec<(u64, u64)>,
        /// `(txn_id, state, last_lsn)` for each active transaction.
        active_txns: Vec<(u64, u8, u64)>,
    },
    /// Marks the end of a checkpoint.
    EndCheckpoint {
        /// Identifier matching the corresponding [`RecordPayload::BeginCheckpoint`].
        checkpoint_id: u64,
    },
    /// A full-page image, written on a page's first modification after a
    /// checkpoint to defend against torn writes.
    FullPageImage {
        /// The page captured.
        page_id: PageId,
        /// Exactly `PAGE_SIZE` bytes.
        image: Vec<u8>,
    },
    /// A page was added to a heap (the durable heap directory). Logged when the
    /// record store allocates a fresh page for a heap; recovery rebuilds the
    /// `heap -> pages` map from these so heaps survive restart.
    HeapPage {
        /// The heap (table/collection/namespace) object id.
        heap_id: u64,
        /// The page now belonging to that heap.
        page_id: PageId,
    },
}

// ── Encoding ────────────────────────────────────────────────────────────────

/// Serialize a payload into `out`, returning its type discriminator.
pub fn encode_body(payload: &RecordPayload, out: &mut Vec<u8>) -> u8 {
    match payload {
        RecordPayload::Insert {
            page_id,
            slot_id,
            after_image,
        } => {
            put_u64(out, page_id.as_u64());
            put_u16(out, *slot_id);
            put_bytes_u32(out, after_image);
            T_INSERT
        }
        RecordPayload::Update {
            page_id,
            slot_id,
            before_image,
            after_image,
        } => {
            put_u64(out, page_id.as_u64());
            put_u16(out, *slot_id);
            put_bytes_u32(out, before_image);
            put_bytes_u32(out, after_image);
            T_UPDATE
        }
        RecordPayload::Delete {
            page_id,
            slot_id,
            before_image,
        } => {
            put_u64(out, page_id.as_u64());
            put_u16(out, *slot_id);
            put_bytes_u32(out, before_image);
            T_DELETE
        }
        RecordPayload::Commit {
            commit_micros,
            flags,
        } => {
            put_i64(out, *commit_micros);
            put_u32(out, *flags);
            T_COMMIT
        }
        RecordPayload::Abort => T_ABORT,
        RecordPayload::Clr {
            page_id,
            slot_id,
            undo_image,
            undo_next_lsn,
        } => {
            put_u64(out, page_id.as_u64());
            put_u16(out, *slot_id);
            put_bytes_u32(out, undo_image);
            put_u64(out, *undo_next_lsn);
            T_CLR
        }
        RecordPayload::BeginCheckpoint { checkpoint_id } => {
            put_u64(out, *checkpoint_id);
            T_BEGIN_CHECKPOINT
        }
        RecordPayload::CheckpointContents {
            dirty_pages,
            active_txns,
        } => {
            put_u32(out, dirty_pages.len() as u32);
            for (page_id, rec_lsn) in dirty_pages {
                put_u64(out, *page_id);
                put_u64(out, *rec_lsn);
            }
            put_u32(out, active_txns.len() as u32);
            for (txn_id, state, last_lsn) in active_txns {
                put_u64(out, *txn_id);
                out.push(*state);
                put_u64(out, *last_lsn);
            }
            T_CHECKPOINT_CONTENTS
        }
        RecordPayload::EndCheckpoint { checkpoint_id } => {
            put_u64(out, *checkpoint_id);
            T_END_CHECKPOINT
        }
        RecordPayload::FullPageImage { page_id, image } => {
            put_u64(out, page_id.as_u64());
            put_bytes_u32(out, image);
            T_FULL_PAGE_IMAGE
        }
        RecordPayload::HeapPage { heap_id, page_id } => {
            put_u64(out, *heap_id);
            put_u64(out, page_id.as_u64());
            T_HEAP_PAGE
        }
    }
}

/// Build a complete on-disk frame: header + body + trailing CRC32.
pub fn assemble_frame(lsn: Lsn, txn_id: u64, prev_lsn: u64, rtype: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(RECORD_HEADER_SIZE + body.len() + RECORD_CRC_SIZE);
    put_u64(&mut out, lsn.as_u64());
    put_u32(&mut out, body.len() as u32);
    out.push(rtype);
    out.extend_from_slice(&[0u8; 3]); // reserved
    put_u64(&mut out, txn_id);
    put_u64(&mut out, prev_lsn);
    out.extend_from_slice(body);
    let crc = crc32fast::hash(&out); // over header + body
    put_u32(&mut out, crc);
    out
}

/// Encode a whole record (convenience; re-encodes the body).
pub fn encode_record(lsn: Lsn, record: &LogRecord) -> Vec<u8> {
    let mut body = Vec::new();
    let rtype = encode_body(&record.payload, &mut body);
    assemble_frame(lsn, record.txn_id, record.prev_lsn, rtype, &body)
}

// ── Decoding ────────────────────────────────────────────────────────────────

/// Decode one frame from the front of `frame`, returning the record and the
/// total bytes consumed (`32 + body_len + 4`).
///
/// Returns [`WalError::CrcMismatch`] if the trailing CRC does not match — the
/// signal recovery uses to stop at a torn write.
pub fn decode_record(frame: &[u8]) -> Result<(Lsn, LogRecord, usize)> {
    if frame.len() < RECORD_HEADER_SIZE {
        return Err(WalError::Decode("frame shorter than header".into()));
    }
    let lsn = get_u64(frame, 0);
    let body_len = get_u32(frame, 8) as usize;
    let rtype = frame[12];
    let txn_id = get_u64(frame, 16);
    let prev_lsn = get_u64(frame, 24);

    let total = RECORD_HEADER_SIZE + body_len + RECORD_CRC_SIZE;
    if frame.len() < total {
        return Err(WalError::Decode(
            "frame shorter than declared length".into(),
        ));
    }
    let crc_pos = RECORD_HEADER_SIZE + body_len;
    let stored_crc = get_u32(frame, crc_pos);
    let actual_crc = crc32fast::hash(&frame[..crc_pos]);
    if stored_crc != actual_crc {
        return Err(WalError::CrcMismatch);
    }

    let payload = decode_body(rtype, &frame[RECORD_HEADER_SIZE..crc_pos])?;
    Ok((
        Lsn(lsn),
        LogRecord {
            txn_id,
            prev_lsn,
            payload,
        },
        total,
    ))
}

fn decode_body(rtype: u8, body: &[u8]) -> Result<RecordPayload> {
    let mut r = Reader::new(body);
    let payload = match rtype {
        T_INSERT => RecordPayload::Insert {
            page_id: PageId(r.u64()?),
            slot_id: r.u16()?,
            after_image: r.bytes_u32()?,
        },
        T_UPDATE => RecordPayload::Update {
            page_id: PageId(r.u64()?),
            slot_id: r.u16()?,
            before_image: r.bytes_u32()?,
            after_image: r.bytes_u32()?,
        },
        T_DELETE => RecordPayload::Delete {
            page_id: PageId(r.u64()?),
            slot_id: r.u16()?,
            before_image: r.bytes_u32()?,
        },
        T_COMMIT => RecordPayload::Commit {
            commit_micros: r.i64()?,
            flags: r.u32()?,
        },
        T_ABORT => RecordPayload::Abort,
        T_CLR => RecordPayload::Clr {
            page_id: PageId(r.u64()?),
            slot_id: r.u16()?,
            undo_image: r.bytes_u32()?,
            undo_next_lsn: r.u64()?,
        },
        T_BEGIN_CHECKPOINT => RecordPayload::BeginCheckpoint {
            checkpoint_id: r.u64()?,
        },
        T_CHECKPOINT_CONTENTS => {
            let dirty_count = r.u32()? as usize;
            let mut dirty_pages = Vec::with_capacity(dirty_count);
            for _ in 0..dirty_count {
                dirty_pages.push((r.u64()?, r.u64()?));
            }
            let active_count = r.u32()? as usize;
            let mut active_txns = Vec::with_capacity(active_count);
            for _ in 0..active_count {
                active_txns.push((r.u64()?, r.u8()?, r.u64()?));
            }
            RecordPayload::CheckpointContents {
                dirty_pages,
                active_txns,
            }
        }
        T_END_CHECKPOINT => RecordPayload::EndCheckpoint {
            checkpoint_id: r.u64()?,
        },
        T_FULL_PAGE_IMAGE => RecordPayload::FullPageImage {
            page_id: PageId(r.u64()?),
            image: r.bytes_u32()?,
        },
        T_HEAP_PAGE => RecordPayload::HeapPage {
            heap_id: r.u64()?,
            page_id: PageId(r.u64()?),
        },
        other => return Err(WalError::UnknownRecordType(other)),
    };
    Ok(payload)
}

// ── Little-endian write helpers ───────────────────────────────────────────

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_i64(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_bytes_u32(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

fn get_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn get_u64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

// ── Little-endian bounds-checked reader for record bodies ──────────────────

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .p
            .checked_add(n)
            .filter(|&e| e <= self.b.len())
            .ok_or_else(|| WalError::Decode("record body truncated".into()))?;
        let slice = &self.b[self.p..end];
        self.p = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Result<u64> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(u64::from_le_bytes(a))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }
    fn bytes_u32(&mut self) -> Result<Vec<u8>> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn roundtrip(rec: &LogRecord) {
        let lsn = Lsn::from_parts(3, 4096);
        let frame = encode_record(lsn, rec);
        let (got_lsn, got, total) = decode_record(&frame).unwrap();
        assert_eq!(got_lsn, lsn);
        assert_eq!(&got, rec);
        assert_eq!(total, frame.len());
    }

    #[test]
    fn roundtrip_each_variant() {
        roundtrip(&LogRecord::txn(
            42,
            Lsn::from_parts(3, 100),
            RecordPayload::Insert {
                page_id: PageId(9),
                slot_id: 5,
                after_image: vec![1, 2, 3, 4],
            },
        ));
        roundtrip(&LogRecord::txn(
            42,
            Lsn::ZERO,
            RecordPayload::Update {
                page_id: PageId(9),
                slot_id: 5,
                before_image: vec![1, 2],
                after_image: vec![3, 4, 5],
            },
        ));
        roundtrip(&LogRecord::txn(
            7,
            Lsn::ZERO,
            RecordPayload::Delete {
                page_id: PageId(1),
                slot_id: 0,
                before_image: vec![9; 30],
            },
        ));
        roundtrip(&LogRecord::txn(
            7,
            Lsn::ZERO,
            RecordPayload::Commit {
                commit_micros: 1_700_000_000_000_000,
                flags: 0,
            },
        ));
        roundtrip(&LogRecord::txn(7, Lsn::ZERO, RecordPayload::Abort));
        roundtrip(&LogRecord::system(RecordPayload::BeginCheckpoint {
            checkpoint_id: 99,
        }));
        roundtrip(&LogRecord::system(RecordPayload::CheckpointContents {
            dirty_pages: vec![(1, 64), (2, 200)],
            active_txns: vec![(5, 1, 100), (6, 2, 150)],
        }));
        roundtrip(&LogRecord::system(RecordPayload::FullPageImage {
            page_id: PageId(12),
            image: vec![0xAB; 256],
        }));
        roundtrip(&LogRecord::system(RecordPayload::HeapPage {
            heap_id: 1234,
            page_id: PageId(56),
        }));
    }

    #[test]
    fn corrupt_crc_is_detected() {
        let mut frame = encode_record(
            Lsn::from_parts(0, 64),
            &LogRecord::txn(1, Lsn::ZERO, RecordPayload::Abort),
        );
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        assert!(matches!(decode_record(&frame), Err(WalError::CrcMismatch)));
    }

    #[test]
    fn unknown_type_is_rejected() {
        let mut frame = assemble_frame(Lsn::from_parts(0, 64), 1, 0, 0x7F, &[]);
        // Recompute CRC so we exercise the type check, not the CRC check.
        let crc_pos = frame.len() - RECORD_CRC_SIZE;
        let crc = crc32fast::hash(&frame[..crc_pos]);
        frame[crc_pos..].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            decode_record(&frame),
            Err(WalError::UnknownRecordType(0x7F))
        ));
    }

    proptest! {
        #[test]
        fn arbitrary_records_roundtrip(
            txn in any::<u64>(),
            prev in any::<u64>(),
            page in any::<u64>(),
            slot in any::<u16>(),
            before in proptest::collection::vec(any::<u8>(), 0..200),
            after in proptest::collection::vec(any::<u8>(), 0..200),
        ) {
            let rec = LogRecord {
                txn_id: txn,
                prev_lsn: prev,
                payload: RecordPayload::Update {
                    page_id: PageId(page),
                    slot_id: slot,
                    before_image: before,
                    after_image: after,
                },
            };
            let frame = encode_record(Lsn::from_parts(1, 64), &rec);
            let (_, got, total) = decode_record(&frame).unwrap();
            prop_assert_eq!(got, rec);
            prop_assert_eq!(total, frame.len());
        }
    }
}
