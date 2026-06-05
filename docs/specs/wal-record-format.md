# Specification: WAL Record Format

**Status:** Accepted (normative)
**Last updated:** 2026-05-15
**Version:** 1.0

This document specifies the on-disk byte layout of WAL records. All multi-byte integers are little-endian unless otherwise noted.

## Segment file format

A WAL segment is a file of fixed size (default 16 MiB) named `prism.wal.<u64_segment_id>` where the segment ID is zero-padded to 20 digits (e.g., `prism.wal.00000000000000000042`).

```
┌──────────────────────────────────────────────────┐
│ SegmentHeader (64 bytes)                         │
├──────────────────────────────────────────────────┤
│ Record 1                                         │
├──────────────────────────────────────────────────┤
│ Record 2                                         │
├──────────────────────────────────────────────────┤
│ ...                                              │
├──────────────────────────────────────────────────┤
│ Zero padding (if any) to segment size            │
└──────────────────────────────────────────────────┘
```

Pre-allocated to full size with `fallocate`/`ftruncate` and fsync'd before being written to. Records are written sequentially from the segment header onward; the unused tail is zero.

### Segment header

```
Offset  Size  Field              Description
─────   ────  ─────              ─────────────────────────────────────
0       8     magic              "PRSMWAL\0"
8       4     format_version     u32 = 1
12      4     segment_size       u32, bytes
16      8     segment_id         u64
24      8     first_lsn          LSN of the first record in this segment
32      8     created_at_micros  i64
40      8     prev_segment_id    For chain validation; 0 for the first
48      4     crc32              CRC32 of bytes 0..44
52      12    reserved
```

Total 64 bytes. The remainder of the segment (`segment_size - 64`) holds records.

## Record format

Each record is:

```
┌──────────────────────────────────────────────────┐
│ RecordHeader (32 bytes)                          │
├──────────────────────────────────────────────────┤
│ RecordBody (variable, body_length bytes)         │
└──────────────────────────────────────────────────┘
```

### Record header (32 bytes)

```
Offset  Size  Field          Description
─────   ────  ─────          ─────────────────────────────────────
0       8     lsn            Monotonic identifier (file offset is
                             implicit; lsn includes offset and segment)
8       4     body_length    Body length in bytes (not including header)
12      1     record_type    Discriminator (see below)
13      3     reserved
16      8     txn_id         TxnId producing this record (0 for checkpoint records)
24      8     prev_lsn       Previous record by same txn (0 if first)
                             Used by undo and CLR resumption
```

### Record body CRC

The body is followed by a 4-byte CRC32 over the record header + body. So the actual on-disk record size is `32 + body_length + 4` bytes.

To find the next record: starting at offset O of the current record, the next record starts at `O + 32 + body_length + 4`.

### LSN encoding

```
lsn: u64 = (segment_id: u32) << 32 | (offset_in_segment: u32)
```

This gives 4 billion segments, each up to 4 GiB, totaling 16 EiB of WAL — effectively unlimited.

When reading by LSN: derive segment_id from high 32 bits, open that segment, seek to offset_in_segment, read.

## Record types

### 0x01 — Insert

```
page_id:        u64
slot_id:        u16
after_image:    u32 length + bytes
```

`after_image` is the full record bytes (24-byte record header + payload). Replaying inserts re-inserts the record at the given slot.

### 0x02 — Update

```
page_id:        u64
slot_id:        u16
before_image:   u32 length + bytes
after_image:    u32 length + bytes
```

Both images included for undo support. before_image is the record bytes before the update; after_image is after.

### 0x03 — Delete

```
page_id:        u64
slot_id:        u16
before_image:   u32 length + bytes
```

Records the pre-delete bytes for undo. After delete, the slot's record has `xmax = txn_id`.

### 0x10 — Commit

```
commit_micros:  i64    (wall-clock commit time)
flags:          u32    (reserved)
```

Marks the transaction durable. Recovery treats txns with a Commit record as winners.

### 0x11 — Abort

```
(no body beyond header)
```

Marks the transaction aborted. Recovery treats txns with an Abort record as already-aborted (no undo needed).

### 0x20 — CLR (Compensation Log Record)

```
page_id:        u64
slot_id:        u16
undo_image:     u32 length + bytes
undo_next_lsn:  u64    (where the next-to-undo record is, or 0 if done)
```

Written during undo. `undo_image` contains the bytes to write back to the page (the before-image of the record being undone). `undo_next_lsn` tells the next recovery where to resume undoing this txn.

### 0x30 — BeginCheckpoint

```
checkpoint_id:  u64
```

Marks the start of a checkpoint. The corresponding CheckpointContents and EndCheckpoint follow.

### 0x31 — CheckpointContents

```
dirty_page_count:    u32
dirty_pages:         [(page_id: u64, rec_lsn: u64)] × dirty_page_count
active_txn_count:    u32
active_txns:         [(txn_id: u64, state: u8, last_lsn: u64)] × active_txn_count
```

The dirty page table and active transaction table at checkpoint time. Recovery uses these to bootstrap its analysis.

### 0x32 — EndCheckpoint

```
checkpoint_id:  u64
```

Marks the end of a checkpoint. Recovery looks for the most recent EndCheckpoint to determine the last completed checkpoint LSN.

### 0x40 — IndexInsert

```
page_id:    u64    (the index leaf or hash bucket page)
key:        u16 length + bytes
rid:        u64
```

### 0x41 — IndexDelete

```
page_id:    u64
key:        u16 length + bytes
rid:        u64
```

### 0x42 — PageSplit (B+tree)

```
left_page:      u64
right_page:     u64
pivot_key:      u16 length + bytes
high_key:       u16 length + bytes
parent_page:    u64    (where the pivot was inserted)
parent_slot:    u16
```

Logged when a B+tree page splits. Replay reconstructs the new page structure.

### 0x43 — BucketSplit (hash)

```
old_bucket:    u64
new_bucket:    u64
new_local_depth: u8
```

### 0x44 — DirectoryExpand (hash)

```
new_global_depth: u8
```

### 0x50 — FullPageImage

```
page_id:        u64
image:          u32 length + bytes    (exactly PAGE_SIZE bytes)
```

Written for any page on its first modification after each checkpoint, to defend against torn writes. Replay overwrites the page.

### 0x80 — UserDefined / Extension

Reserved for future extensions. The body format is opaque to the WAL but understood by some higher-level component. v1 does not use this; it exists so v2 can add record types without bumping the format version.

## CRC

Every record carries a 4-byte CRC32 (zlib polynomial) over the 32-byte header + body. Validated on every read during replay.

A CRC mismatch is treated as end-of-log: all subsequent bytes in this and following segments are discarded. This is the torn-write handling rule.

## Endianness

All multi-byte integers little-endian. Strings (e.g., keys, names) are byte sequences with explicit length prefixes; no implicit terminators.

## Versioning

The segment header's `format_version` governs the entire WAL format. Version 1 is what is documented here. The engine refuses to open WAL segments with a different version.

## Size considerations

Typical record sizes:
- Insert of a 100-byte tuple: 32 (header) + 8 + 2 + 4 + 124 (after_image) + 4 (CRC) = 174 bytes.
- Update of the same tuple: 32 + 8 + 2 + 4 + 124 + 4 + 124 + 4 = 302 bytes.
- Commit: 32 + 8 + 4 + 4 = 48 bytes.
- Full-page image: 32 + 8 + 4 + 8192 + 4 = 8240 bytes (one per page per checkpoint cycle in the worst case).

A typical 1000-tps insert-only workload writes about 170 KB/sec of WAL plus ~50 KB of commit records — easily within SSD bandwidth.

## References

- ADR 0003 — recovery design.
- `components/wal.md` — operational details.
- `components/recovery.md` — replay logic.
- Mohan et al. 1992 — ARIES paper.
