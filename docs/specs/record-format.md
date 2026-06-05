# Specification: Record Format

**Status:** Accepted (normative)
**Last updated:** 2026-05-15
**Version:** 1.0

This document specifies the byte layout of records stored in slotted pages. The format is uniform across the three access methods; only the payload bytes differ. All multi-byte integers are little-endian unless otherwise noted.

## Record header (24 bytes)

```
Offset  Size  Field          Description
─────   ────  ─────          ───────────────────────────────────────
0       8     xmin           TxnId that created this version
8       8     xmax           TxnId that deleted/superseded this version
                             (0 = current, not deleted)
16      6     prev_version   RecordId of the previous version in the
                             chain (NIL = 0xFFFFFFFFFFFF)
22      2     flags          See below
```

Total: 24 bytes.

### Flags field

```
Bit  Name              Meaning
───  ────              ───────
0    HAS_PREV_VERSION  prev_version is valid (otherwise NIL)
1    FORWARDED         payload is a forwarding RID (see below)
2    TOMBSTONE         deleted, payload may be meaningless
3    INFOMASK_LOCKED   row currently write-locked (advisory)
4-15 reserved
```

### TxnId encoding

`TxnId` is a `u64`. Reserved values:
- `0`: sentinel for "no transaction" (used for xmax = 0 meaning "not deleted").
- `1`: bootstrap transaction (used only during database creation).
- TxnIds 2 through 2^64 - 1: user transactions.

### prev_version encoding

`prev_version` is a 6-byte little-endian `RecordId`:
- Bytes 0-5 are the low 48 bits of the RecordId.
- The high 16 bits (slot_id) come from a flags field — actually no.

Correction: a `RecordId` is 64 bits total. To fit in 6 bytes (48 bits), we restrict prev_version to record IDs within the same database file with `page_id < 2^32` and `slot_id < 2^16`. Specifically: 4 bytes page_id, 2 bytes slot_id, totaling 6 bytes.

For databases larger than 2^32 pages (= 2^32 × 8 KiB = 32 TiB), v2 will extend this field. v1 caps at 32 TiB practical size; documented limitation.

NIL is represented as `0xFFFFFFFFFFFF` (all 1 bits, six bytes).

## Forwarding records

When an update produces a record too large for its current slot, the storage layer may write a forwarding record at the original location:

```
RecordHeader (24 bytes):
  xmin, xmax, prev_version: as normal
  flags: FORWARDED bit set
Payload (8 bytes):
  new_rid: u64
```

A reader resolving the original RID sees the FORWARDED flag, reads new_rid from the payload, and re-fetches at the new location (one indirection hop maximum; chained forwarding is not allowed).

## Relational payload

The payload of a record in a SQL heap.

```
Offset  Size                Field
─────   ────                ─────
0       ceil(N/8)           null_bitmap (N = number of columns, bit i = column i is null)
varies  variable            fixed-width columns, in declared order
varies  num_var_cols × 2    variable-width offset array (offsets relative to payload start)
varies  variable            variable-width column data
```

### Column types and encoding

| Type | Width | Encoding |
|---|---|---|
| `Bool` | 1 byte | 0 = false, 1 = true |
| `Int32` | 4 bytes | little-endian two's complement |
| `Int64` | 8 bytes | little-endian two's complement |
| `Float32` | 4 bytes | IEEE 754 single, little-endian |
| `Float64` | 8 bytes | IEEE 754 double, little-endian |
| `Timestamp` | 8 bytes | i64 microseconds since Unix epoch (UTC) |
| `Text` | variable | UTF-8 bytes; length from offset array |
| `Blob` | variable | raw bytes; length from offset array |

### Null bitmap

Bit i (counting from LSB of byte i/8) is 1 if column i is NULL, 0 otherwise. Null columns occupy zero bytes in the data sections; the offset array's entry for a null variable column is the same as the previous offset (zero length).

### Variable-width offset array

For each variable-width column in declared order, a 16-bit little-endian offset relative to the payload start indicates where the column's data begins. Length is implied by the next offset (or the record's end).

### Example

Table `users(id INT32, email TEXT, active BOOL, age INT64)`.

A row `(42, 'a@b.com', true, NULL)`:

```
null_bitmap:    [0b0000_1000]                  // bit 3 set (age is null)
fixed_section:  [0x2A, 0x00, 0x00, 0x00,       // id = 42
                 0x01,                          // active = true
                 (age omitted, null)]
offset_array:   [0x0A, 0x00]                   // email starts at offset 10
var_data:       [0x61, 0x40, 0x62, 0x2E,       // 'a', '@', 'b', '.'
                 0x63, 0x6F, 0x6D]              // 'c', 'o', 'm'
```

Total payload: 1 + 4 + 1 + 2 + 7 = 15 bytes (plus 24-byte record header = 39 bytes).

## Document payload

The payload of a record in a document collection.

```
Offset  Size  Field
─────   ────  ─────
0       4     doc_length      Total document length (this field + remainder)
4       2     field_count
6       N     fields          (N = doc_length - 6)
```

### Field encoding

```
Offset  Size      Field
─────   ────      ─────
0       1         type_tag
1       2         name_length
3       name_len  name (UTF-8, no null terminator)
varies  varies    value (encoding depends on type_tag)
```

### Type tags

```
Tag   Type        Value encoding
───   ────        ──────────────
0x00  Null        (no value bytes)
0x01  Bool        1 byte (0 or 1)
0x02  Int32       4 bytes little-endian
0x03  Int64       8 bytes little-endian
0x04  Double      8 bytes IEEE 754 little-endian
0x05  String      u32 length + UTF-8 bytes
0x06  Binary      u32 length + u8 subtype + bytes
0x07  Array       u32 length + nested document (field names are stringified indices "0","1",...)
0x08  Object      u32 length + nested document
0x09  Timestamp   i64 microseconds since epoch
0x0A  ObjectId    12 bytes (random + timestamp + counter)
0x0B  Decimal     reserved for v2
```

### ObjectId encoding

12 bytes:
- 4 bytes: timestamp (seconds since epoch, big-endian, for sortability)
- 5 bytes: random (process-startup random seed mixed with PID)
- 3 bytes: counter (incremented per ObjectId generated in this process, big-endian)

Big-endian for the timestamp and counter so ObjectIds sort chronologically as byte strings.

### Example

A document `{ "name": "Alice", "age": 30 }`:

```
doc_length:    [0x21, 0x00, 0x00, 0x00]        // 33 bytes total
field_count:   [0x02, 0x00]
field[0]:
  type_tag:    [0x05]                          // String
  name_len:    [0x04, 0x00]
  name:        [0x6E, 0x61, 0x6D, 0x65]         // "name"
  value_len:   [0x05, 0x00, 0x00, 0x00]         // 5 bytes
  value:       [0x41, 0x6C, 0x69, 0x63, 0x65]   // "Alice"
field[1]:
  type_tag:    [0x02]                          // Int32
  name_len:    [0x03, 0x00]
  name:        [0x61, 0x67, 0x65]               // "age"
  value:       [0x1E, 0x00, 0x00, 0x00]         // 30
```

Total: 33 bytes (payload) + 24 (header) = 57 bytes.

## KV payload

```
Offset  Size      Field
─────   ────      ─────
0       2         key_length
2       key_len   key (opaque bytes)
varies  rest      value (opaque bytes)
```

Value length is implied: `value_length = total_payload_length - 2 - key_length`.

## Index entry format (B+tree leaf)

Index entries are stored in B+tree leaf pages, not in record-store records. Their layout is in `specs/page-format.md` under "B+tree leaf page." Briefly:

```
slot:    key_offset, key_length, rid
keydata: variable bytes
```

The rid points at the record-store record. Visibility is checked there, not at the index entry.

## Catalog row encoding

Catalog rows (in `_prism_tables`, etc.) use the relational payload format with the schemas given in `components/catalog.md`. They are records like any other; nothing in the storage layer treats them specially.

## Size limits

- Maximum record size (including 24-byte header): `PAGE_SIZE - 32 (page header) - 4 (one slot entry) - 32 (margin)` = approximately 8124 bytes. Records larger than this are rejected at insert with `RecordTooLarge`.
- Maximum string/blob in a column: same limit, accounting for the row's other columns.
- Maximum document size: 4 MiB declared limit (but bounded by record size in v1; documents larger than ~8 KiB are rejected). Larger documents are post-v1.
- Maximum KV key: 1024 bytes.
- Maximum KV value: bounded by record size (~8 KiB minus overhead).

These bounds will be relaxed in v2 via overflow pages (TOAST-style).

## Versioning

The record header layout is version 1. Future format versions are documented separately and migration paths are described.

## References

- ADR 0005 — unified record format decision.
- `specs/page-format.md` — containing layout.
- `components/mvcc.md` — uses xmin/xmax/prev_version.
- `components/sql-engine.md`, `components/document-engine.md`, `components/kv-engine.md` — each access method's payload interpreter.
