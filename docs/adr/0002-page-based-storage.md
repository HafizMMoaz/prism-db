# ADR 0002: Page-based storage with slotted pages

**Status:** Accepted
**Date:** 2026-05-15

## Context

The storage layer's first decision is the unit of I/O and in-memory caching. Three families dominate:

1. **Page-based (fixed-size blocks).** The classic relational approach. PostgreSQL, MySQL/InnoDB, SQLite, Oracle, SQL Server. Pages of 4 KiB to 64 KiB read and written as units. Records are placed inside pages by a slotted layout.

2. **Log-structured merge trees (LSM).** RocksDB, LevelDB, Cassandra, ScyllaDB. Writes go to memtables, are flushed as sorted SSTables, and compacted in the background. Reads merge across levels.

3. **Append-only or copy-on-write trees.** LMDB, CouchDB. Every write produces a new B-tree node; the old version remains. Garbage collected eventually.

The choice affects every layer above it: how the buffer pool works, how WAL records are formatted, how indexes are organized, how concurrent access is structured.

## Decision

Prism uses **page-based storage with slotted pages**, 8 KiB page size. One heap file per database. Slotted layout for variable-length records.

## Alternatives considered

### LSM trees
**For:** Excellent write throughput, sequential I/O for SSTable writes, well-suited to write-heavy workloads, mature in Rust (`sled`, RocksDB-rs, `fjall`).

**Against:** The read path involves merging across N levels, which complicates point lookups and is awkward for secondary indexes (an LSM secondary index has its own LSM tree, doubling write amplification). Range scans across levels require careful tombstone handling. Compaction is a major operational concern - bad compaction tuning causes stalls. For an OLTP workload with point reads and small writes, the write amplification benefits are smaller than for analytics or time-series.

Most importantly: representing three data models - relational, document, KV - uniformly is harder on LSM. Relational tables with multiple secondary indexes are not LSM's strength. Documents with field-based indexes likewise. KV alone would be fine on LSM, but we are not building a KV-only engine.

### Append-only / copy-on-write
**For:** No WAL needed - the storage itself is the log. Simple recovery semantics (the last good root pointer). LMDB is famously fast for reads.

**Against:** Every write touches the root-to-leaf path of the tree, producing high write amplification. Long-running readers can prevent garbage collection, leading to file growth. The model is best suited to read-heavy workloads with few writers. We expect write-heavy workloads too (KV namespaces, document inserts).

Also: CoW trees couple the storage format and the index format tightly. We want to separate the heap (record storage) from the indexes (B+tree, hash) so that updates to a record don't require updating its physical position in every index.

### Page-based with slotted pages (chosen)
**For:** Decouples physical storage (pages) from logical access (indexes that point to RIDs). Records can be updated in place when they fit; only growth requires forwarding pointers or relocation. WAL is straightforward: log the page change. Recovery is standard ARIES. Buffer pool semantics are universally understood. Compatible with all three data models because pages are content-agnostic.

The model has decades of engineering depth: Postgres's heap, InnoDB's pages, SQL Server's pages. We are not inventing; we are building on a well-understood foundation.

**Against:** Random I/O for cache misses. Write amplification at the page level - modifying one byte writes 8 KiB to the WAL (mitigated by physiological logging, which logs the change, not the page). Internal fragmentation when records don't pack neatly. Page splits when a record no longer fits.

## Page size: why 8 KiB

Common page sizes: 4 KiB (SQLite, Postgres default), 8 KiB (Postgres recommended for SSDs, InnoDB default), 16 KiB (InnoDB), 32 KiB (some analytical systems).

- 4 KiB matches the OS page size and most SSD logical block sizes, minimizing partial-write risk.
- 8 KiB amortizes header overhead better and matches the underlying erase block patterns of modern SSDs more closely. Postgres uses 8 KiB by default.
- 16 KiB and larger pack more tuples per page but increase the I/O unit, which hurts point reads.

We choose 8 KiB as a balance: large enough to amortize page header costs and pack reasonable numbers of tuples; small enough that random page reads do not waste bandwidth. Page size is a compile-time constant for v1; making it runtime-configurable adds complexity without a clear win.

Note: 8 KiB pages do not guarantee atomic write at the storage layer. Most SSDs have a 4 KiB atomic write granularity. Page checksums detect torn writes; full-page-image WAL logging on first modification after checkpoint repairs them. See `components/wal.md`.

## Slotted page layout: why

Records in a slotted page are variable-length. The page header contains pointers to slots; slots contain pointers to record data. Slot array grows from one end of the page, record data grows from the other; free space is the gap between.

This layout has properties we need:
- Records are addressed by `(PageId, SlotId)` - the slot ID is stable even when records are reorganized within the page (we update the slot's offset, not the slot's ID).
- Variable-length records pack densely.
- Tuples can be deleted in place: zero the slot pointer, leave the data until the next page compaction.
- Updates that fit replace the record in place. Updates that grow can be handled by writing a forwarding RID (one indirection) or by relocating the record entirely if it has no inbound RID references.

The detailed layout is in `specs/page-format.md`.

## Consequences

### Enabled
- Standard buffer pool semantics: fetch page, pin, modify, mark dirty, unpin.
- Standard ARIES recovery: WAL records reference `(PageId, SlotId)`.
- Records of all three models share the same physical storage format.
- B+tree and hash indexes can point to records via stable RIDs.
- Page checksums provide local corruption detection.

### Constrained
- Page size is fixed at 8 KiB.
- A single record cannot exceed page size. Large records (TOAST-style overflow pages) are a v2 feature; v1 caps record size at page size minus header overhead (approximately 8000 bytes).
- The heap file grows in 8 KiB increments. File truncation is rare; deleted space becomes free for new records, not returned to the OS.

### Required follow-on decisions
- Buffer pool replacement policy → ADR 0007.
- WAL record format → ADR 0003.
- MVCC tuple format → ADR 0004 and ADR 0005.

## References

- Hellerstein, Stonebraker, Hamilton: "Architecture of a Database System," Foundations and Trends in Databases, 2007. (Section 4 on storage management.)
- PostgreSQL documentation: page structure, item pointers.
- `specs/page-format.md` - normative byte layout.
