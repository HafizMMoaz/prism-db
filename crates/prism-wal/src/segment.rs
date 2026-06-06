//! WAL segment files: the 64-byte segment header and file naming.
//!
//! Byte layout is normative; see `docs/specs/wal-record-format.md`.

use crate::Lsn;
use crate::error::{Result, WalError};

/// Size of the segment header, in bytes.
pub const SEGMENT_HEADER_SIZE: usize = 64;
/// The WAL on-disk format version.
pub const WAL_FORMAT_VERSION: u32 = 1;

const SEG_MAGIC: [u8; 8] = *b"PRSMWAL\0";

// Segment header field offsets.
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 8;
const OFF_SEGMENT_SIZE: usize = 12;
const OFF_SEGMENT_ID: usize = 16;
const OFF_FIRST_LSN: usize = 24;
const OFF_CREATED_AT: usize = 32;
const OFF_PREV_SEGMENT_ID: usize = 40;
const OFF_CRC: usize = 48;
const CRC_BODY_LEN: usize = 44; // CRC covers bytes [0, 44)

/// The filename for segment `id`: `prism.wal.<20-digit zero-padded id>`.
pub fn segment_file_name(id: u64) -> String {
    format!("prism.wal.{id:020}")
}

/// Parse a segment id from a filename, if it matches the segment naming scheme.
pub fn parse_segment_id(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("prism.wal.")?;
    if digits.len() == 20 && digits.bytes().all(|b| b.is_ascii_digit()) {
        digits.parse().ok()
    } else {
        None
    }
}

/// A parsed WAL segment header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentHeader {
    /// On-disk format version.
    pub format_version: u32,
    /// Fixed size of this segment, in bytes.
    pub segment_size: u32,
    /// This segment's id.
    pub segment_id: u64,
    /// LSN of the first record in this segment.
    pub first_lsn: u64,
    /// Creation time, microseconds since the Unix epoch.
    pub created_at_micros: i64,
    /// The previous segment's id (0 for the first), for chain validation.
    pub prev_segment_id: u64,
}

impl SegmentHeader {
    /// The LSN at which records begin in this segment.
    pub fn first_record_lsn(&self) -> Lsn {
        Lsn(self.first_lsn)
    }

    /// Encode the header into its fixed 64-byte form, including its CRC32.
    pub fn encode(&self) -> [u8; SEGMENT_HEADER_SIZE] {
        let mut buf = [0u8; SEGMENT_HEADER_SIZE];
        buf[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(&SEG_MAGIC);
        buf[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&self.format_version.to_le_bytes());
        buf[OFF_SEGMENT_SIZE..OFF_SEGMENT_SIZE + 4]
            .copy_from_slice(&self.segment_size.to_le_bytes());
        buf[OFF_SEGMENT_ID..OFF_SEGMENT_ID + 8].copy_from_slice(&self.segment_id.to_le_bytes());
        buf[OFF_FIRST_LSN..OFF_FIRST_LSN + 8].copy_from_slice(&self.first_lsn.to_le_bytes());
        buf[OFF_CREATED_AT..OFF_CREATED_AT + 8]
            .copy_from_slice(&self.created_at_micros.to_le_bytes());
        buf[OFF_PREV_SEGMENT_ID..OFF_PREV_SEGMENT_ID + 8]
            .copy_from_slice(&self.prev_segment_id.to_le_bytes());
        let crc = crc32fast::hash(&buf[..CRC_BODY_LEN]);
        buf[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Decode and validate a segment header from the start of `buf`.
    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < SEGMENT_HEADER_SIZE {
            return Err(WalError::Decode("segment shorter than header".into()));
        }
        if buf[OFF_MAGIC..OFF_MAGIC + 8] != SEG_MAGIC {
            return Err(WalError::BadMagic);
        }
        let stored_crc = u32::from_le_bytes([
            buf[OFF_CRC],
            buf[OFF_CRC + 1],
            buf[OFF_CRC + 2],
            buf[OFF_CRC + 3],
        ]);
        if stored_crc != crc32fast::hash(&buf[..CRC_BODY_LEN]) {
            return Err(WalError::Decode("segment header CRC mismatch".into()));
        }
        let format_version = rd_u32(buf, OFF_VERSION);
        if format_version != WAL_FORMAT_VERSION {
            return Err(WalError::IncompatibleVersion(format_version));
        }
        Ok(Self {
            format_version,
            segment_size: rd_u32(buf, OFF_SEGMENT_SIZE),
            segment_id: rd_u64(buf, OFF_SEGMENT_ID),
            first_lsn: rd_u64(buf, OFF_FIRST_LSN),
            created_at_micros: rd_u64(buf, OFF_CREATED_AT) as i64,
            prev_segment_id: rd_u64(buf, OFF_PREV_SEGMENT_ID),
        })
    }
}

fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_parse_roundtrip() {
        assert_eq!(segment_file_name(42), "prism.wal.00000000000000000042");
        assert_eq!(parse_segment_id("prism.wal.00000000000000000042"), Some(42));
        assert_eq!(parse_segment_id("prism.wal.42"), None); // not zero-padded
        assert_eq!(parse_segment_id("notes.txt"), None);
    }

    #[test]
    fn header_roundtrip() {
        let h = SegmentHeader {
            format_version: WAL_FORMAT_VERSION,
            segment_size: 16 * 1024 * 1024,
            segment_id: 5,
            first_lsn: Lsn::from_parts(5, 64).as_u64(),
            created_at_micros: 1_700_000_000_000_000,
            prev_segment_id: 4,
        };
        let buf = h.encode();
        assert_eq!(SegmentHeader::decode(&buf).unwrap(), h);
    }

    #[test]
    fn rejects_bad_magic_and_crc() {
        let h = SegmentHeader {
            format_version: WAL_FORMAT_VERSION,
            segment_size: 4096,
            segment_id: 0,
            first_lsn: 64,
            created_at_micros: 0,
            prev_segment_id: 0,
        };
        let mut buf = h.encode();
        buf[0] = b'X';
        assert!(matches!(
            SegmentHeader::decode(&buf),
            Err(WalError::BadMagic)
        ));

        let mut buf2 = h.encode();
        buf2[OFF_SEGMENT_ID] ^= 0xFF; // body change without re-checksum
        assert!(matches!(
            SegmentHeader::decode(&buf2),
            Err(WalError::Decode(_))
        ));
    }
}
