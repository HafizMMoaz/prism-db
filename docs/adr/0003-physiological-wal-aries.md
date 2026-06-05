# ADR 0003: Physiological WAL with ARIES recovery

**Status:** Accepted
**Date:** 2026-05-15

## Context

Durability requires that committed transactions survive process crashes and that aborted or in-flight transactions leave no partial effects. The standard mechanism is the write-ahead log: every change is described in a log record, the log is `fsync`'d to disk before the corresponding page can be flushed, and on restart the log is replayed to reconstruct the correct state.

The log can take several forms:

1. **Physical logging.** Each log record describes a page byte-range change. Replay overwrites bytes. Simplest to implement, largest log volume.

2. **Logical logging.** Each log record describes an operation ("insert tuple X into table T"). Replay re-executes the operation. Smallest log volume, hardest to make correct under concurrent recovery, requires the system to be in a consistent state to replay.

3. **Physiological logging.** Each log record describes an operation **against a specific page** ("insert tuple X at slot S of page P"). Replay applies the operation to the page directly. The page provides the consistency context; the operation provides the compactness.

The recovery algorithm built on the log is also a choice. ARIES (Mohan et al. 1992) is the dominant production design and consists of three phases:

- **Analysis:** scan the log from the last completed checkpoint forward, rebuild the dirty page table and active transaction table at crash time.
- **Redo:** scan forward from the earliest dirty page LSN, re-apply every log record whose LSN is greater than the page's current LSN.
- **Undo:** for each transaction that was active at crash time and did not commit, walk its log records backward and apply the inverse, writing Compensation Log Records (CLRs) so that recovery itself is idempotent.

ARIES is well-understood, used in DB2, SQL Server, InnoDB, and many other engines. The alternatives are research-grade or substantially simpler but less robust.

## Decision

Prism uses **physiological logging** and **ARIES recovery**:

- Log records reference a specific page and slot, plus before-image and after-image bytes.
- Recovery has three phases: analysis, redo, undo, with CLRs for crash-safe undo.
- Fuzzy checkpointing: checkpoints record state without halting writers.

## Alternatives considered

### Pure physical logging
**For:** Trivial to implement — log the bytes that changed, replay the bytes.

**Against:** Large log volume because each log record carries page byte ranges. For a small insert that adds a tuple, the log record contains the inserted bytes and the slot directory delta; physical logging would log both as raw byte changes. Physiological logging produces a smaller record describing the insert operation against the page.

More importantly: physical logging doesn't handle index page splits well. A split is a structural change involving multiple pages; logging each byte change is correct but verbose. Physiological logging can log "split page P at key K, produce P and P'" as a single operation.

### Pure logical logging
**For:** Smallest log volume. Operations are expressed at the table or index level: "insert (1, 'foo') into table T."

**Against:** Replay requires the system to be in a consistent state. If a crash happened mid-page-split, the table or index is structurally inconsistent. Logical replay cannot proceed without first repairing the structure, which requires lower-level information. The model also struggles with concurrent operations: the log order doesn't capture page-level conflicts.

Postgres uses logical replication on top of physical/physiological WAL for streaming, but the recovery WAL itself is physiological. This is a well-trodden trade-off.

### Simpler "no UNDO" recovery (NO-STEAL, FORCE)
**For:** Eliminates the undo phase entirely. The buffer pool refuses to flush dirty pages of uncommitted transactions (NO-STEAL); on commit, all dirty pages are forced to disk before commit completes (FORCE).

**Against:** Severely limits concurrency — long-running transactions pin pages in the buffer pool. FORCE on commit is incompatible with high throughput because every commit waits for synchronous page writes. ARIES with STEAL/NO-FORCE is the engineering standard precisely because it decouples commit latency from page write latency.

### Single-page logging without checkpointing
**For:** Simplest. Recovery scans the entire log on startup.

**Against:** Recovery time grows linearly with log size. A database that has accumulated weeks of operations would take hours to recover. Checkpoints bound recovery time.

## Why physiological + ARIES

1. **Correctness is well-understood.** ARIES is the most-cited recovery algorithm in database history. Failure modes are catalogued. Reference implementations exist.

2. **Compatible with STEAL/NO-FORCE.** We want to flush dirty pages of uncommitted transactions (for buffer pool reclaim) and we want commit to be cheap (only the log flushes synchronously). ARIES is designed for exactly this.

3. **Composes with MVCC.** Visibility logic lives above the WAL; the WAL just durably records page changes. MVCC tuple versions are page contents from the WAL's perspective.

4. **Composes with cross-model transactions.** Log records reference pages, not models. A transaction modifying a SQL table page and a document collection page writes two log records under one TxnId; recovery treats them identically.

## Log record format

Every log record carries:

- `lsn`: 64-bit monotonic identifier, allocated by the log writer.
- `prev_lsn`: the previous log record for the same transaction. Forms a per-transaction backward chain used during undo.
- `txn_id`: which transaction.
- `record_type`: discriminator (Insert, Update, Delete, Commit, Abort, CLR, Checkpoint, etc.).
- `page_id`: the affected page (zero for non-page records like Commit).
- `slot_id`: the affected slot (when applicable).
- `before_image`: bytes for undo (omitted for redo-only records).
- `after_image`: bytes for redo (omitted for undo-only records).
- `crc`: CRC32 of the record body.

The byte-level layout is in `specs/wal-record-format.md`.

## The WAL invariant

The fundamental rule: **a dirty page may not be flushed to disk until the WAL record describing its modification is durable.**

Mechanically: every page carries a `page_lsn` (the LSN of the most recent log record that modified it). The buffer pool, before flushing page P, calls `wal.flush_through(P.page_lsn)`. The WAL ensures all log records up to and including that LSN are fsync'd. Only then is the page written.

This invariant is what makes redo work: after a crash, every page on disk has a `page_lsn` such that all log records up to `page_lsn` are also on disk. Redo replays from `page_lsn + 1` forward.

## Group commit

`fsync` is expensive — milliseconds even on NVMe. The WAL batches concurrent commits: when transaction T calls `flush_through(L)`, the writer notes the request, waits a tiny budget (microseconds), and `fsync`s all log up to the current write pointer at once. All in-flight commits return together.

## Fuzzy checkpointing

A checkpoint is a marker after which recovery analysis can start. Periodically:

1. Write `BEGIN_CHECKPOINT` log record.
2. Snapshot the active transaction table and the dirty page table.
3. Write a `CHECKPOINT_CONTENTS` log record with these snapshots.
4. Write `END_CHECKPOINT` log record.
5. Persist a "last completed checkpoint LSN" pointer (in a fixed location).

Writers continue running throughout. Recovery starts from the last completed checkpoint's `BEGIN_CHECKPOINT` LSN, uses the contents to bootstrap the analysis phase, and proceeds normally.

## Consequences

### Enabled
- Crash recovery to a consistent state from any point.
- Cross-model atomicity (recovery is model-agnostic).
- High commit throughput via group commit.
- Buffer pool freedom (STEAL: can flush dirty pages of uncommitted txns).
- Low commit latency (NO-FORCE: commit doesn't wait for page flush).

### Constrained
- Every page modification produces a log record. Write-heavy workloads pay this overhead.
- Log records carry before-images, doubling log volume vs redo-only logs. Justified by abort and rollback support.
- Recovery is single-threaded in v1. Parallel recovery is a v2 optimization.
- Log archiving is required for point-in-time recovery; this is operational complexity for the user.

### Required follow-on decisions
- WAL on-disk format → `specs/wal-record-format.md`.
- Group commit batching parameters → `components/wal.md`.
- Checkpoint frequency → operational tunable, default 5 minutes or 64 MiB of log, whichever comes first.

## References

- Mohan, Haderle, Lindsay, Pirahesh, Schwarz: "ARIES: A Transaction Recovery Method Supporting Fine-Granularity Locking and Partial Rollbacks Using Write-Ahead Logging." ACM TODS, 1992. The foundational paper.
- Gray and Reuter: *Transaction Processing: Concepts and Techniques.* 1992. Chapters 9-11.
- PostgreSQL WAL documentation.
- ADR 0002 (page-based storage) — log records reference pages.
- ADR 0006 (single WAL across models) — the unification depends on this design.
- `components/recovery.md` — operational details.
