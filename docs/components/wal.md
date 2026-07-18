# Component: Write-Ahead Log

**Crate:** `prism-wal`
**Status:** Accepted
**Last updated:** 2026-05-15

## Purpose

The WAL is the durable record of every page mutation. It is the foundation of crash recovery and the only mechanism that gives committed transactions their durability guarantee. Every change to the database goes through the WAL before it goes anywhere else.

## Public interface

```rust
pub struct Wal { /* opaque */ }

impl Wal {
    pub fn open(path: &Path, config: Config) -> Result<Self>;

    /// Append a log record. Returns the LSN assigned to it. Not yet durable.
    pub fn append(&self, record: LogRecord) -> Result<Lsn>;

    /// Block until all records with LSN <= up_to are durable.
    /// Cheap if already durable; expensive (fsync) if not.
    pub fn flush_through(&self, up_to: Lsn) -> Result<()>;

    /// Return the current durable LSN. Records up to and including this are on disk.
    pub fn durable_lsn(&self) -> Lsn;

    /// Iterate records starting from a given LSN. Used by recovery.
    pub fn replay(&self, from: Lsn) -> impl Iterator<Item = Result<(Lsn, LogRecord)>>;
}
```

## On-disk format

WAL files are append-only segments, 16 MiB each, named `prism.wal.<segment_id>`. New segments are pre-allocated and `fdatasync`'d before being written to.

Within a segment:

```
┌────────────────────────────────────────────────────┐
│ Segment header (64 bytes)                          │
│   magic, segment_id, first_lsn, page_size, etc.    │
├────────────────────────────────────────────────────┤
│ Record 1                                           │
│ ┌──────────────────────────────────────────┐       │
│ │ Frame header (16 bytes)                  │       │
│ │   record_len: u32                        │       │
│ │   record_type: u8                        │       │
│ │   reserved: 3 bytes                      │       │
│ │   crc32: u32                             │       │
│ │   lsn: u64                               │       │
│ └──────────────────────────────────────────┘       │
│ Record body (variable)                             │
├────────────────────────────────────────────────────┤
│ Record 2                                           │
│ ...                                                │
└────────────────────────────────────────────────────┘
```

The frame header is fixed-size and self-locating, so a damaged record body does not prevent the next record from being found (we can scan forward by `record_len`).

Detailed byte layout: `specs/wal-record-format.md`.

## Record types

| Type | Body |
|---|---|
| `Insert` | `txn`, `page`, `slot`, `after_image`, `prev_lsn` |
| `Update` | `txn`, `page`, `slot`, `before_image`, `after_image`, `prev_lsn` |
| `Delete` | `txn`, `page`, `slot`, `before_image`, `prev_lsn` |
| `Commit` | `txn`, `commit_ts`, `prev_lsn` |
| `Abort` | `txn`, `prev_lsn` |
| `Clr` | `txn`, `page`, `slot`, `undo_image`, `undo_next_lsn`, `prev_lsn` |
| `BeginCheckpoint` | (marker only) |
| `CheckpointContents` | `dirty_page_table`, `active_txn_table` |
| `EndCheckpoint` | (marker only) |
| `PageSplit` | (B+tree structural; pages and pivot key) |
| `IndexInsert` | (B+tree key insert; key, RID, page) |
| `IndexDelete` | (B+tree key delete; key, RID, page) |

`prev_lsn` is the LSN of the previous WAL record produced by the same transaction. Forms a backward chain used by undo; allows skipping unrelated records during undo of a single transaction.

## LSN allocation

LSN is monotonically increasing 64-bit:

```rust
struct LsnAllocator {
    next: AtomicU64,
}
```

Allocation is atomic add. LSNs are never reused, even across restarts (we persist the high-water mark in the WAL's last segment header).

## Group commit

`flush_through(L)` is the synchronous path for transaction commit. It must be both correct and fast.

Design:

```rust
struct WalWriter {
    in_memory_buffer: parking_lot::Mutex<RingBuffer>,
    durable_lsn: AtomicU64,
    flush_request: parking_lot::Mutex<Option<Lsn>>,
    flush_cond: parking_lot::Condvar,
    writer_thread: JoinHandle<()>,
}
```

Append path (called by transaction operations):
1. Serialize record into the in-memory buffer under the buffer mutex.
2. Capture the assigned LSN.
3. Release the mutex.
4. Return immediately. The record is in memory; not yet durable.

Flush path (called by `flush_through`):
1. Read `durable_lsn`. If `durable_lsn >= up_to`, return.
2. Update `flush_request` if `up_to` > current request.
3. Wait on `flush_cond` until `durable_lsn >= up_to`.

Writer thread:
```
loop:
    wait on flush_cond OR timeout 1ms
    if flush_request is set OR buffer has data:
        snapshot buffer contents
        write to current segment file
        fdatasync segment file
        update durable_lsn atomically
        broadcast flush_cond
```

Multiple concurrent commits arriving in a 1 ms window batch into one fsync. Under high concurrency, hundreds of commits can share a single fsync, dramatically improving throughput.

The 1 ms timeout is configurable. Trade-off: shorter timeout = lower commit latency under low load; longer timeout = better batching under high load.

## Segment rotation

When the active segment reaches its size limit (16 MiB):

1. Allocate the next segment file (pre-fallocate to 16 MiB).
2. fdatasync the new file.
3. Switch the writer to the new segment.
4. The old segment is closed and eligible for archive/deletion based on retention policy.

Pre-allocation prevents the I/O hiccup of extending a file while a commit is in flight.

## Checkpoint coordination

The checkpointer (a separate thread, runs every 5 minutes or 64 MiB of WAL, whichever first):

1. Calls `wal.append(BeginCheckpoint)`. Gets LSN `B`.
2. Snapshots: `buffer_pool.snapshot_dirty_pages()`, `txn_manager.snapshot_active_txns()`.
3. Calls `wal.append(CheckpointContents { dirty_pages, active_txns })`. Gets LSN `C`.
4. Calls `wal.append(EndCheckpoint)`. Gets LSN `E`.
5. Calls `wal.flush_through(E)`.
6. Writes the value `B` to the database header's "last checkpoint" field. fdatasyncs the header. Recovery starts from `B` on next start.

The dirty page table snapshot lets recovery bound the redo phase: it starts redo at `min(page_lsn for page in dirty_pages)`.

## Replay (recovery)

`wal.replay(from)` yields records sequentially. Recovery (see `components/recovery.md`) consumes this iterator three times:

1. Analysis: forward scan, rebuild active txn table and dirty page table.
2. Redo: forward scan from the earliest dirty page LSN, replay every record.
3. Undo: backward through loser transactions' prev_lsn chains.

The iterator is restartable: it can be reset to any LSN via `replay(lsn)`.

## CRC validation

Every record's body has a CRC32. On replay, mismatched CRC means torn write at this offset. Recovery treats this as end-of-log: everything before is valid, everything from this point is discarded. This is safe because anything after a torn write was not committed (the commit record's fsync would have completed only after this record's fsync).

## Concurrency

- `append` is thread-safe: multiple appenders can run concurrently, serialized only by the in-memory buffer mutex (which is short-held).
- `flush_through` is thread-safe and idempotent.
- The writer thread is the only owner of the file descriptor for writing.
- Replay is single-threaded by contract; called only during recovery.

## Configuration

```toml
[wal]
directory = "/var/lib/prism/wal"
segment_size_mib = 16
group_commit_window_ms = 1
sync_mode = "fdatasync"     # or "fsync" or "none" (test only)
retain_segments = 32        # for point-in-time recovery
```

`sync_mode = "none"` is explicitly for testing; using it in production loses durability and we emit a warning at startup if set.

## Metrics

- `prism_wal_appends_total`
- `prism_wal_flushes_total` (number of fsync calls)
- `prism_wal_batched_commits_per_flush` (histogram)
- `prism_wal_flush_latency_seconds`
- `prism_wal_bytes_written_total`
- `prism_wal_segment_rotations_total`

## Failure modes

| Failure | Behavior |
|---|---|
| fsync returns EIO | Engine panics. The OS may have lost data; we don't trust further writes. The classical "fsync gate" failure. |
| Disk full | New appends fail with `WalDiskFull`. Recovery is still possible after the operator frees space. |
| Segment file corruption mid-replay | Recovery stops at the corruption point; reports the LSN. Operator must intervene (typically restore from backup). |
| Writer thread panic | Engine panics. The WAL is single-thread for writes; if the writer is gone, nothing can commit. |

## References

- ADR 0003 - recovery algorithm.
- ADR 0006 - single WAL across models.
- `specs/wal-record-format.md` - byte layout.
- `components/recovery.md` - the consumer.
- PostgreSQL WAL: `src/backend/access/transam/xlog.c`.
- ARIES paper (Mohan et al. 1992).
