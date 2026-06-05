# Glossary

**Status:** Accepted
**Last updated:** 2026-05-15

Terms used throughout the Prism design corpus, defined once here. When a document uses one of these terms, it means exactly what is below.

---

**Access method.** The code path that translates user-facing operations on a particular data model into operations on the underlying record store. Prism has three: relational (SQL), document, and key-value. Distinct from the storage method (slotted pages, the same for all access methods).

**ARIES.** Algorithms for Recovery and Isolation Exploiting Semantics. The recovery algorithm Prism implements, described in Mohan et al. 1992. Three phases: analysis, redo, undo. See ADR 0003 and `components/recovery.md`.

**Buffer pool.** The fixed-size in-memory cache of pages. Pages are fetched from disk on miss, evicted under pressure, and pinned while in use to prevent eviction. See `components/buffer-pool.md`.

**Catalog.** System tables describing user-visible objects: tables, columns, indexes, collections, namespaces. Stored in the same engine as user data, bootstrapped at database creation time.

**Checkpoint.** A point in the WAL after which recovery does not need to scan. Prism uses fuzzy checkpoints: the dirty page table and active transaction table are snapshotted without stopping writers. See `components/wal.md`.

**CLR.** Compensation Log Record. A WAL record written during undo to make recovery idempotent across crashes during recovery. If the system crashes mid-undo, the CLRs already written tell the next recovery pass where to resume.

**Cross-model transaction.** A transaction whose operations span more than one access method (e.g., inserts a SQL row and updates a document atomically). The defining feature of Prism. See ADR 0006.

**Dirty page.** A page whose in-memory contents differ from its on-disk contents. The dirty page table tracks all dirty pages and is critical for checkpointing and recovery.

**Frame.** A slot in the buffer pool holding one page. Pages move into and out of frames; the frame is the physical container.

**Fuzzy checkpoint.** A checkpoint taken without halting writers. Begins with a `BEGIN_CHECKPOINT` log record capturing the active transaction table and dirty page table, ends with an `END_CHECKPOINT` log record marking it complete. Recovery starts from the most recent completed checkpoint.

**Heap file.** A file on disk holding pages. Prism uses one heap file per database. Pages within the file are addressed by page ID, which is `(file_id, page_offset)` — though with one file per database, file ID is constant.

**Idempotent.** An operation that produces the same final state regardless of how many times it is applied. Recovery operations must be idempotent because crashes during recovery cause partial replays.

**LSN.** Log Sequence Number. A monotonically increasing 64-bit identifier for WAL records. Every WAL record has a unique LSN. Pages store the LSN of the most recent log record that modified them; this is the basis of redo's idempotence.

**MVCC.** Multi-Version Concurrency Control. The concurrency control scheme Prism uses. Writers create new versions of tuples rather than overwriting; readers see a snapshot defined by their start time. See ADR 0004.

**Page.** The unit of disk I/O and buffer pool management. 8 KiB in Prism. Slotted layout: header, slot array growing from one end, tuple data growing from the other.

**Pin.** A claim on a page in the buffer pool preventing eviction. Pages must be pinned before reading or writing their contents. Pin counts are tracked per frame; eviction is allowed only when pin count is zero.

**Record.** A tuple of bytes in a slotted page, addressed by `(page_id, slot_id)` = `RecordId`. All three access methods store their data as records; the access methods differ in how they interpret the bytes.

**RecordId (RID).** A 64-bit identifier for a record: 48 bits of page ID and 16 bits of slot ID. Stable for the lifetime of the record. Updates that grow a record past its slot's capacity produce a forwarding pointer (a new RID); old RID continues to resolve via the forwarding pointer for one hop.

**Redo.** The phase of recovery that replays log records to bring pages forward to the state they should have been in at crash time. Idempotent: replaying the same log record twice has the same effect as replaying it once.

**Snapshot isolation.** An isolation level in which each transaction sees a snapshot of the database as of its start time. Readers do not block writers; writers do not block readers. Write-write conflicts are detected and one transaction aborts. Does not prevent write-skew anomalies; serializable isolation does, but is not in scope for v1.

**Slotted page.** A page layout in which a header points to a directory of slots, each slot points to a variable-length record, slot array grows from one end of the page, record data grows from the other. See `specs/page-format.md`.

**TxnId.** Transaction identifier. 64-bit monotonically increasing. Assigned at transaction start. Used as `xmin` (creator) and `xmax` (deleter) of tuple versions.

**Undo.** The phase of recovery that rolls back transactions that were active at crash time and did not commit. Walks each loser transaction's log records backward, applies inverse operations, writes CLRs.

**WAL.** Write-Ahead Log. The durable record of every page mutation. Page changes are written to the WAL and `fsync`'d to disk before the corresponding page can be flushed. The fundamental durability invariant: log first, page second. See `components/wal.md`.

**xmin / xmax.** Per-tuple fields holding the transaction IDs that created and deleted the tuple version, respectively. `xmax = 0` means the tuple is current; `xmax != 0` means it was deleted or superseded by that transaction.
