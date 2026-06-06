//! Page 0: the database header.
//!
//! A fixed (non-slotted) layout read and validated at startup. Byte layout is
//! normative; see "Page 0: Database header" in `docs/specs/page-format.md`.

use crate::PAGE_SIZE;
use crate::checksum::crc32;
use crate::error::{Result, StorageError};

/// Magic bytes identifying a Prism database file.
pub const MAGIC: [u8; 8] = *b"PRISMDB\0";
/// The on-disk format version this build reads and writes.
pub const FORMAT_VERSION: u32 = 1;

// Field offsets within page 0.
const OFF_MAGIC: usize = 0;
const OFF_FORMAT_VERSION: usize = 8;
const OFF_PAGE_SIZE: usize = 12;
const OFF_CREATED_AT: usize = 16;
const OFF_LAST_CHECKPOINT_LSN: usize = 24;
const OFF_CLEAN_SHUTDOWN: usize = 32;
const OFF_BOOTSTRAP_TABLES: usize = 40;
const OFF_BOOTSTRAP_COLUMNS: usize = 48;
const OFF_BOOTSTRAP_INDEXES: usize = 56;
const OFF_BOOTSTRAP_COLLECTIONS: usize = 64;
const OFF_BOOTSTRAP_NAMESPACES: usize = 72;
const OFF_BOOTSTRAP_USERS: usize = 80;
const OFF_BOOTSTRAP_GRANTS: usize = 88;
const OFF_BOOTSTRAP_SEQUENCES: usize = 96;
const OFF_NEXT_OID: usize = 104;
const OFF_NEXT_PAGE_ID: usize = 112;

/// The header CRC32 covers bytes `[0, 124)` and is stored at `[124, 128)`.
const OFF_HEADER_CRC: usize = 124;
const HEADER_CRC_BODY_LEN: usize = 124;

/// The parsed database header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbHeader {
    /// On-disk format version.
    pub format_version: u32,
    /// Page size recorded in the file (must equal [`PAGE_SIZE`]).
    pub page_size: u32,
    /// Database creation time, microseconds since the Unix epoch.
    pub created_at_micros: i64,
    /// LSN of the last completed checkpoint.
    pub last_checkpoint_lsn: u64,
    /// Whether the database was last shut down cleanly.
    pub clean_shutdown: bool,
    /// Catalog root RID for `_prism_tables`.
    pub bootstrap_tables_root_rid: u64,
    /// Catalog root RID for `_prism_columns`.
    pub bootstrap_columns_root_rid: u64,
    /// Catalog root RID for `_prism_indexes`.
    pub bootstrap_indexes_root_rid: u64,
    /// Catalog root RID for `_prism_collections`.
    pub bootstrap_collections_root_rid: u64,
    /// Catalog root RID for `_prism_namespaces`.
    pub bootstrap_namespaces_root_rid: u64,
    /// Catalog root RID for `_prism_users`.
    pub bootstrap_users_root_rid: u64,
    /// Catalog root RID for `_prism_grants`.
    pub bootstrap_grants_root_rid: u64,
    /// Catalog root RID for `_prism_sequences`.
    pub bootstrap_sequences_root_rid: u64,
    /// The next object id to allocate.
    pub next_oid: u64,
    /// The next page id to allocate.
    pub next_page_id: u64,
}

impl DbHeader {
    /// Create a fresh header for a brand-new database created at `created_at_micros`.
    ///
    /// Bootstrap RIDs and counters start at zero; the catalog layer fills them in.
    pub fn new(created_at_micros: i64) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            page_size: PAGE_SIZE as u32,
            created_at_micros,
            last_checkpoint_lsn: 0,
            clean_shutdown: true,
            bootstrap_tables_root_rid: 0,
            bootstrap_columns_root_rid: 0,
            bootstrap_indexes_root_rid: 0,
            bootstrap_collections_root_rid: 0,
            bootstrap_namespaces_root_rid: 0,
            bootstrap_users_root_rid: 0,
            bootstrap_grants_root_rid: 0,
            bootstrap_sequences_root_rid: 0,
            next_oid: 0,
            next_page_id: 0,
        }
    }

    /// Encode the header into a full page-0 buffer, including its CRC32.
    pub fn encode(&self) -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
        buf[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(&MAGIC);
        wr_u32(&mut buf, OFF_FORMAT_VERSION, self.format_version);
        wr_u32(&mut buf, OFF_PAGE_SIZE, self.page_size);
        wr_i64(&mut buf, OFF_CREATED_AT, self.created_at_micros);
        wr_u64(&mut buf, OFF_LAST_CHECKPOINT_LSN, self.last_checkpoint_lsn);
        buf[OFF_CLEAN_SHUTDOWN] = u8::from(self.clean_shutdown);
        wr_u64(
            &mut buf,
            OFF_BOOTSTRAP_TABLES,
            self.bootstrap_tables_root_rid,
        );
        wr_u64(
            &mut buf,
            OFF_BOOTSTRAP_COLUMNS,
            self.bootstrap_columns_root_rid,
        );
        wr_u64(
            &mut buf,
            OFF_BOOTSTRAP_INDEXES,
            self.bootstrap_indexes_root_rid,
        );
        wr_u64(
            &mut buf,
            OFF_BOOTSTRAP_COLLECTIONS,
            self.bootstrap_collections_root_rid,
        );
        wr_u64(
            &mut buf,
            OFF_BOOTSTRAP_NAMESPACES,
            self.bootstrap_namespaces_root_rid,
        );
        wr_u64(&mut buf, OFF_BOOTSTRAP_USERS, self.bootstrap_users_root_rid);
        wr_u64(
            &mut buf,
            OFF_BOOTSTRAP_GRANTS,
            self.bootstrap_grants_root_rid,
        );
        wr_u64(
            &mut buf,
            OFF_BOOTSTRAP_SEQUENCES,
            self.bootstrap_sequences_root_rid,
        );
        wr_u64(&mut buf, OFF_NEXT_OID, self.next_oid);
        wr_u64(&mut buf, OFF_NEXT_PAGE_ID, self.next_page_id);

        let crc = crc32(&buf[..HEADER_CRC_BODY_LEN]);
        wr_u32(&mut buf, OFF_HEADER_CRC, crc);
        buf
    }

    /// Decode and validate a page-0 buffer.
    ///
    /// Returns [`StorageError::IncompatibleDatabase`] on magic/version/page-size
    /// mismatch and [`StorageError::ChecksumMismatch`] on a bad CRC.
    pub fn decode(buf: &[u8; PAGE_SIZE]) -> Result<Self> {
        if buf[OFF_MAGIC..OFF_MAGIC + 8] != MAGIC {
            return Err(StorageError::IncompatibleDatabase(
                "bad magic bytes (not a Prism database file)".into(),
            ));
        }
        let stored_crc = rd_u32(buf, OFF_HEADER_CRC);
        let actual_crc = crc32(&buf[..HEADER_CRC_BODY_LEN]);
        if stored_crc != actual_crc {
            return Err(StorageError::ChecksumMismatch(0));
        }

        let format_version = rd_u32(buf, OFF_FORMAT_VERSION);
        if format_version != FORMAT_VERSION {
            return Err(StorageError::IncompatibleDatabase(format!(
                "format version {format_version} (this build supports {FORMAT_VERSION})"
            )));
        }
        let page_size = rd_u32(buf, OFF_PAGE_SIZE);
        if page_size as usize != PAGE_SIZE {
            return Err(StorageError::IncompatibleDatabase(format!(
                "page size {page_size} (this build uses {PAGE_SIZE})"
            )));
        }

        Ok(Self {
            format_version,
            page_size,
            created_at_micros: rd_i64(buf, OFF_CREATED_AT),
            last_checkpoint_lsn: rd_u64(buf, OFF_LAST_CHECKPOINT_LSN),
            clean_shutdown: buf[OFF_CLEAN_SHUTDOWN] != 0,
            bootstrap_tables_root_rid: rd_u64(buf, OFF_BOOTSTRAP_TABLES),
            bootstrap_columns_root_rid: rd_u64(buf, OFF_BOOTSTRAP_COLUMNS),
            bootstrap_indexes_root_rid: rd_u64(buf, OFF_BOOTSTRAP_INDEXES),
            bootstrap_collections_root_rid: rd_u64(buf, OFF_BOOTSTRAP_COLLECTIONS),
            bootstrap_namespaces_root_rid: rd_u64(buf, OFF_BOOTSTRAP_NAMESPACES),
            bootstrap_users_root_rid: rd_u64(buf, OFF_BOOTSTRAP_USERS),
            bootstrap_grants_root_rid: rd_u64(buf, OFF_BOOTSTRAP_GRANTS),
            bootstrap_sequences_root_rid: rd_u64(buf, OFF_BOOTSTRAP_SEQUENCES),
            next_oid: rd_u64(buf, OFF_NEXT_OID),
            next_page_id: rd_u64(buf, OFF_NEXT_PAGE_ID),
        })
    }
}

fn wr_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn wr_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}
fn wr_i64(b: &mut [u8], o: usize, v: i64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[o..o + 4]);
    u32::from_le_bytes(a)
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}
fn rd_i64(b: &[u8], o: usize) -> i64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    i64::from_le_bytes(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DbHeader {
        let mut h = DbHeader::new(1_700_000_000_000_000);
        h.last_checkpoint_lsn = 42;
        h.clean_shutdown = false;
        h.bootstrap_tables_root_rid = 0x1122_3344_5566_7788;
        h.bootstrap_sequences_root_rid = 9;
        h.next_oid = 1000;
        h.next_page_id = 3;
        h
    }

    #[test]
    fn encode_decode_roundtrip() {
        let h = sample();
        let buf = h.encode();
        let back = DbHeader::decode(&buf).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = sample().encode();
        buf[0] = b'X';
        // Re-checksum so we exercise the magic check specifically, not the CRC.
        let crc = crc32(&buf[..HEADER_CRC_BODY_LEN]);
        buf[OFF_HEADER_CRC..OFF_HEADER_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            DbHeader::decode(&buf),
            Err(StorageError::IncompatibleDatabase(_))
        ));
    }

    #[test]
    fn rejects_corrupt_crc() {
        let mut buf = sample().encode();
        buf[OFF_NEXT_OID] ^= 0xFF; // body change without re-checksumming
        assert!(matches!(
            DbHeader::decode(&buf),
            Err(StorageError::ChecksumMismatch(0))
        ));
    }

    #[test]
    fn rejects_wrong_version() {
        let mut buf = sample().encode();
        wr_u32(&mut buf, OFF_FORMAT_VERSION, 999);
        let crc = crc32(&buf[..HEADER_CRC_BODY_LEN]);
        buf[OFF_HEADER_CRC..OFF_HEADER_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            DbHeader::decode(&buf),
            Err(StorageError::IncompatibleDatabase(_))
        ));
    }
}
