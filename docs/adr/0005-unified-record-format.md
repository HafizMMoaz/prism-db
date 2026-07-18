# ADR 0005: Unified record format across models

**Status:** Accepted
**Date:** 2026-05-15

## Context

If the cross-model thesis is to hold, the storage layer must store SQL rows, documents, and KV pairs in a uniform way. Otherwise we have three storage layouts requiring three buffer pool integrations, three WAL paths, and three recovery strategies - which is just three databases in a trench coat.

The question is what the uniform representation should be.

## Decision

All three models store their data as **records** in the same slotted page format. A record is:

```
┌──────────────────────────────────────────────┐
│ RecordHeader (24 bytes)                       │
│   xmin: TxnId (8 bytes)                       │
│   xmax: TxnId (8 bytes)                       │
│   next_version: RecordId (6 bytes) + flags (2)│
├──────────────────────────────────────────────┤
│ Payload (variable, format depends on access   │
│   method)                                     │
└──────────────────────────────────────────────┘
```

The record header is universal: every tuple in every page of every model has these 24 bytes in the same layout. The MVCC machinery operates exclusively on the header. The payload is opaque to the storage layer and the transaction manager.

The three access methods interpret the payload differently:

- **Relational:** payload is a row encoded as `[null_bitmap][fixed_fields][var_offsets][var_data]`. The schema (from the catalog) tells the SQL engine how to decode it.
- **Document:** payload is a tagged binary document (similar to BSON but our own format). Self-describing - no external schema needed.
- **Key-value:** payload is `[key_len: u16][key: bytes][value: bytes]`. The slot's length minus header gives total payload length; key_len splits the rest.

## Alternatives considered

### Three separate physical layouts
Each model has its own page format, optimized for its access patterns.

**Against:** Cross-model transactions become impossible to implement cleanly. The WAL would need three record types per operation. The buffer pool would manage three kinds of pages and have to know which is which. MVCC would need three implementations.

The optimization win (per-model packing efficiency) is real but secondary; the architectural cost is catastrophic.

### One layout but no shared MVCC header
Every model stores its data in slotted pages but xmin/xmax live somewhere else (a side table, an external version store, an LSN-only header).

**Against:** Visibility checks become two-lookup operations: fetch the record, then look up its version metadata. This doubles the work on the hottest path in the engine.

The shared MVCC header costs 24 bytes per tuple. For a 100-byte tuple, this is 24% overhead - significant. For a 1 KiB tuple, 2.4% - negligible. Most realistic workloads have tuples larger than 100 bytes; the overhead is acceptable.

### Self-describing payload everywhere (BSON-like for all)
Even SQL rows would be stored as tagged binary documents.

**For:** Schema evolution becomes trivial (add a field, old rows have null/missing).

**Against:** Massive space overhead for relational workloads where the schema is known. A row of three integers shouldn't carry field-name strings. SQL applications expect compact relational storage.

We accept the cost of multiple payload formats. The complexity lives in the access method layer (where it belongs), not in the storage layer.

## Record identifier (RecordId / RID)

Every record is addressable by a stable 64-bit `RecordId`:

```
RecordId: u64 = (page_id: u48) | (slot_id: u16)
```

- 48 bits of page ID: 2^48 pages × 8 KiB = 2^61 bytes = 2 EiB max database. Enough.
- 16 bits of slot ID: 65,535 slots per page. With 8 KiB pages and ~16-byte minimum record size, max slots is around 500, so 16 bits is generous.

RIDs are stable for the lifetime of the record version. When a record is updated and the new version fits in the same slot, the RID is unchanged. When it grows past the slot, the slot is converted to a forwarding pointer (a new RID), and the old RID continues to resolve via one hop of indirection. This is documented in `specs/page-format.md`.

## What payload formats look like

### Relational payload
```
[null_bitmap: ceil(n_cols / 8) bytes]
[fixed_width_cols: packed]
[var_width_offsets: n_var_cols × 2 bytes]
[var_width_data: bytes]
```

The schema (column count, types, order) comes from the catalog by table OID. The catalog is itself stored in tables with a bootstrap schema.

### Document payload
```
[total_len: u32]
[field_count: u16]
[fields: { tag: u8, name_len: u16, name: bytes, value_len_or_inline: depends on tag, value: bytes }]
```

Tags include `Null`, `Bool`, `Int32`, `Int64`, `Double`, `String`, `Binary`, `Array`, `Object`, `Timestamp`, `ObjectId`. Detailed in `specs/record-format.md`.

### KV payload
```
[key_len: u16]
[key: bytes]
[value: bytes]
```

Value length is implied by total record length minus header minus key length.

## Tuple header details

```rust
#[repr(C)]
struct RecordHeader {
    xmin: TxnId,       // 8 bytes
    xmax: TxnId,       // 8 bytes (0 == not deleted)
    next_version: u64, // 6 bytes RID + 2 bytes flags
}
```

Flags include:
- `bit 0`: HAS_NEXT_VERSION - version chain pointer is valid
- `bit 1`: FORWARDED - payload starts with a RID forwarding pointer; resolve through it
- `bit 2`: TOMBSTONE - deleted, payload meaningless (used in some access method internals)
- `bit 3-15`: reserved

The header is `repr(C)` and serialized little-endian. The on-disk byte layout is normative; the in-memory Rust struct is illustrative.

## Index entries

B+tree and hash index entries store `(key, RecordId)`. They are not MVCC-aware themselves; the visibility check happens after the index lookup, against the record's header. This means:

- A B+tree may contain multiple entries pointing at different versions of the same logical row (after an update, the new version has a new RID, both old and new entries can coexist).
- After commit, the executor / DML path is responsible for removing stale index entries (or marking them invisible).

This is the same approach Postgres uses (Postgres calls it "heap-only tuples" when the optimization applies; we have not implemented the HOT optimization for v1).

## Consequences

### Enabled
- Single buffer pool stores pages from all three models without distinguishing them.
- Single WAL writes records for all three models in one log.
- Single recovery procedure replays them.
- MVCC visibility logic is identical across models.
- Cross-model transactions follow from the shared header.

### Constrained
- 24 bytes of fixed overhead per record. Small KV pairs (10-byte keys, 10-byte values) suffer a high relative overhead. Documented and accepted.
- Records cannot exceed page size (minus header overhead, ~8 KiB). Large-record support (TOAST-style) is post-v1.
- Payload format is per-access-method. Adding a new model requires defining a payload format; the framework is uniform but the format is not free.

### Required follow-on decisions
- Detailed byte layouts → `specs/page-format.md` and `specs/record-format.md`.
- Index entry format → `components/btree.md` and `components/hash-index.md`.

## References

- ADR 0002 - slotted page layout.
- ADR 0004 - xmin/xmax semantics.
- PostgreSQL tuple format documentation.
- `specs/page-format.md` and `specs/record-format.md` - normative byte layouts.
