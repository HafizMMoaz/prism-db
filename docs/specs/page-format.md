# Specification: Page Format

**Status:** Accepted (normative)
**Last updated:** 2026-05-15
**Version:** 1.0

This document specifies the on-disk byte layout of a Prism page. All multi-byte integers are little-endian unless otherwise noted. All offsets are in bytes from the start of the page.

## Page size

`PAGE_SIZE = 8192` (8 KiB). Compile-time constant. Not user-configurable in v1.

## Page header (32 bytes)

```
Offset  Size  Field             Description
─────   ────  ─────             ────────────────────────────────────────
0       8     page_lsn          LSN of last log record modifying this page
8       2     checksum          CRC32 of bytes 16..PAGE_SIZE, low 16 bits
                                (kept compact; 16 bits = ~65535 collisions
                                possible per page, sufficient with WAL
                                checksums providing defense in depth)
10      1     page_type         1=Heap, 2=BTreeInternal, 3=BTreeLeaf,
                                4=HashBucket, 5=HashOverflow, 6=Free
11      1     reserved
12      2     flags             bit 0: has_overflow, others reserved
14      2     free_space_offset Offset where free space starts (low)
16      2     free_space_end    Offset where free space ends (high)
18      2     slot_count        Number of slots in slot array
20      2     reserved
22      2     reserved
24      8     reserved          For future use
```

The page header is 32 bytes; the remaining 8160 bytes are slot array + free space + record data.

Note: the checksum is computed over offsets 16 through PAGE_SIZE-1, **not** including the page_lsn (offsets 0-7) or the checksum field itself (offsets 8-9). This allows the page_lsn to be updated in place under a held write latch without recomputing the checksum, as long as the rest of the page is unchanged. The page_lsn is protected by the WAL (which has its own checksums); the page checksum protects the body.

## Slotted page layout (heap, btree-leaf)

```
┌──────────────────────────────────────────────────┐
│ PageHeader (32 bytes)                            │
├──────────────────────────────────────────────────┤
│ Slot 0    (4 bytes: offset, length)              │
│ Slot 1                                           │
│ Slot 2                                           │
│ ...                                              │
│ Slot N-1                                         │  slot array grows
│                                                  │  downward from 32
├──────────────────────────────────────────────────┤
│                                                  │
│                Free space                        │
│                                                  │
├──────────────────────────────────────────────────┤
│ Record bytes (newest at low offset)              │
│ ...                                              │
│ Record 2                                         │
│ Record 1                                         │
│ Record 0                                         │  record data grows
└──────────────────────────────────────────────────┘  upward from PAGE_SIZE
```

### Slot
```
Offset  Size  Field
─────   ────  ─────
0       2     record_offset   Where record starts in the page (0 = empty slot)
2       2     record_length   Record length in bytes; high bit reserved
                              for "is forwarding pointer" flag
```

`record_offset = 0` means the slot is empty (record has been deleted or never allocated). Slot IDs are stable — when a slot is freed, its ID is not reused immediately; the slot stays empty until the page is compacted (rare).

`record_length` high bit (bit 15) set means the slot is a forwarding pointer; the record bytes hold an 8-byte `RecordId` instead of a normal record. This is how oversized updates that don't fit in place are handled: the original slot becomes a pointer to the new location.

### Free space invariant

```
free_space_offset = 32 + 4 * slot_count
free_space_end    = lowest record_offset among occupied slots
free_space_size   = free_space_end - free_space_offset
```

A new record of size R + 4 (slot overhead) fits iff `free_space_size >= R + 4`.

### Record placement

To insert a record of L bytes:
1. Compute `new_record_offset = free_space_end - L`.
2. Find an empty slot or allocate a new slot (record offset must fit before slot_array_end).
3. Write the record bytes at `new_record_offset`.
4. Set the slot's `record_offset` and `record_length`.
5. Update `free_space_end = new_record_offset`.
6. If new slot was allocated, increment `slot_count` and adjust `free_space_offset`.

### Record deletion

1. Read the slot.
2. Mark the slot as empty: `record_offset = 0`.
3. The record bytes remain in place. They will be reclaimed at the next page compaction.

### Page compaction

When a page becomes fragmented (e.g., free_space_size is small but the sum of unused regions is large), compact:
1. Scan slots in record_offset order.
2. Rewrite records contiguously at the high end of the page.
3. Update slot offsets.
4. Recompute free_space_offset and free_space_end.

Compaction is a single-page operation; it's logged as a single WAL record describing the new page state (a full-page image is simplest). Held under an exclusive page latch.

Compaction is triggered:
- When an insert needs space and `free_space_size < required` but `total_unused > required`.
- Opportunistically by a background process (post-v1).

## Heap page

Records in a heap page have a 24-byte record header followed by the payload. See `specs/record-format.md` for record layout.

The page header's `page_type = 1`.

## B+tree internal page

```
┌──────────────────────────────────────────────────┐
│ PageHeader (32 bytes), page_type = 2             │
├──────────────────────────────────────────────────┤
│ BTreeInternalHeader (16 bytes):                  │
│   level: u16            (1 for level above leaf) │
│   key_count: u16                                 │
│   right_sibling: u64    (PageId)                 │
│   high_key_offset: u16                           │
│   high_key_length: u16                           │
├──────────────────────────────────────────────────┤
│ Slot array: (key_count + 1) × 12 bytes           │
│   each: child_page (u64), key_offset (u16),      │
│         key_length (u16)                         │
├──────────────────────────────────────────────────┤
│ Free space                                       │
├──────────────────────────────────────────────────┤
│ Key data (variable length, packed)               │
│ High key                                         │
└──────────────────────────────────────────────────┘
```

The last slot has `key_offset = 0`, `key_length = 0`; its `child_page` handles values ≥ the last key.

## B+tree leaf page

```
┌──────────────────────────────────────────────────┐
│ PageHeader (32 bytes), page_type = 3             │
├──────────────────────────────────────────────────┤
│ BTreeLeafHeader (16 bytes):                      │
│   level: u16            (always 0)               │
│   entry_count: u16                               │
│   right_sibling: u64                             │
│   high_key_offset: u16                           │
│   high_key_length: u16                           │
├──────────────────────────────────────────────────┤
│ Slot array: entry_count × 12 bytes               │
│   each: key_offset (u16), key_length (u16),      │
│         rid (u64)                                │
├──────────────────────────────────────────────────┤
│ Free space                                       │
├──────────────────────────────────────────────────┤
│ Key data                                         │
│ High key                                         │
└──────────────────────────────────────────────────┘
```

Leaf entries are sorted by key. Range scans iterate the slot array in order and follow `right_sibling` to continue.

## Hash bucket page

```
┌──────────────────────────────────────────────────┐
│ PageHeader (32 bytes), page_type = 4             │
├──────────────────────────────────────────────────┤
│ HashBucketHeader (16 bytes):                     │
│   local_depth: u8                                │
│   reserved: u8                                   │
│   entry_count: u16                               │
│   overflow_page: u64                             │
│   reserved: 4 bytes                              │
├──────────────────────────────────────────────────┤
│ Entry array: entry_count × 16 bytes              │
│   each: hash (u32), key_offset (u16),            │
│         key_length (u16), rid (u64)              │
├──────────────────────────────────────────────────┤
│ Free space                                       │
├──────────────────────────────────────────────────┤
│ Key data                                         │
└──────────────────────────────────────────────────┘
```

Entries are not sorted (unlike btree leaves) because hash buckets don't preserve order. Entries can be appended on insert; on lookup, scan all entries comparing hash first (fast filter) then key bytes (for collisions).

## Free page

A page that has been allocated but not yet used by any heap or index. `page_type = 6`. Contents are zero. Free pages are tracked in a free-page list (a chain of pages with `next_free_page` pointers in a fixed header location).

## Page 0: Database header

Page 0 is reserved as the database header. It has its own format, not a slotted page:

```
Offset  Size  Field
─────   ────  ─────
0       8     magic              "PRISMDB\0" (bytes 0x50,0x52,0x49,0x53,0x4D,0x44,0x42,0x00)
8       4     format_version     u32, current = 1
12      4     page_size          u32, = 8192
16      8     created_at_micros  i64, microseconds since Unix epoch
24      8     last_checkpoint_lsn u64
32      8     last_clean_shutdown u8 (0 or 1), reserved 7 bytes
40      8     bootstrap_tables_root_rid    (catalog root for _prism_tables)
48      8     bootstrap_columns_root_rid
56      8     bootstrap_indexes_root_rid
64      8     bootstrap_collections_root_rid
72      8     bootstrap_namespaces_root_rid
80      8     bootstrap_users_root_rid
88      8     bootstrap_grants_root_rid
96      8     bootstrap_sequences_root_rid
104     8     next_oid           u64
112     8     next_page_id       u64
120     8     reserved
128            reserved bytes through PAGE_SIZE
```

The database header has its own CRC32 stored at offsets 124-127 (last 4 bytes of the header proper). The remainder of the page is zero-filled.

## Endianness and alignment

All multi-byte integers are little-endian. Records are not guaranteed any particular alignment beyond byte alignment; readers must use unaligned reads (or copy to aligned buffers). Rust's `from_le_bytes` and `to_le_bytes` are the canonical accessors.

## Versioning

The `format_version` field on page 0 governs the entire on-disk format. Version 1 is documented here. A future version 2 will document its changes and the migration path. Mixed-version files are not supported; the engine refuses to open a file with a different version.

## References

- ADR 0002 — slotted page choice.
- ADR 0005 — record header layout.
- `specs/record-format.md` — record byte layout.
- `components/disk-manager.md` — page I/O.
- `components/btree.md`, `components/hash-index.md` — index page consumers.
