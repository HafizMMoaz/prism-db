# Component: Buffer Pool

**Crate:** `prism-buffer`
**Status:** Accepted
**Last updated:** 2026-05-15

## Purpose

The buffer pool is the in-memory cache of pages. It owns a fixed pool of frames; pages are loaded into frames on demand, pinned during use, and evicted under pressure. The buffer pool enforces the WAL invariant: a dirty page may not leave memory until its log records are durable.

This is the second-most-contended component in the engine (after the WAL writer). The design optimizes for the common case (cache hit on a pinned page) at the cost of complexity on the cold path.

## Public interface

```rust
pub struct BufferPool { /* opaque */ }

impl BufferPool {
    pub fn new(disk: Arc<DiskManager>, wal: Arc<Wal>, config: Config) -> Result<Self>;

    /// Pin a page for reading. Returns a guard that unpins on drop.
    pub fn fetch_read(&self, page_id: PageId) -> Result<PageReadGuard>;

    /// Pin a page for writing. Returns a guard that unpins on drop.
    pub fn fetch_write(&self, page_id: PageId) -> Result<PageWriteGuard>;

    /// Allocate a new page and pin it for writing.
    pub fn new_page(&self) -> Result<PageWriteGuard>;

    /// Flush all dirty pages with page_lsn <= up_to_lsn. Used for checkpoints.
    pub fn flush_through(&self, up_to_lsn: Lsn) -> Result<()>;

    /// Flush every dirty page. Used at clean shutdown.
    pub fn flush_all(&self) -> Result<()>;
}
```

`PageReadGuard` and `PageWriteGuard` are RAII; dropping releases the latch and decrements the pin count.

## Frame structure

```rust
struct Frame {
    data: Box<[u8; PAGE_SIZE]>,    // 8 KiB, aligned for O_DIRECT
    state: parking_lot::Mutex<FrameState>,
    content_latch: parking_lot::RwLock<()>,
}

struct FrameState {
    page_id: Option<PageId>,        // None = empty
    pin_count: u32,
    usage_count: u8,                // 0..=3, clock-sweep counter
    dirty: bool,
    page_lsn: Lsn,                  // LSN of last log record that modified this page
}
```

The `state` mutex is short-held: only for the bookkeeping fields. The `content_latch` is the long-held latch, acquired by callers via the guard objects. Separating them avoids holding the state lock while a long read or write proceeds.

## Page table

A concurrent hash map from `PageId` to `FrameId`:

```rust
page_table: DashMap<PageId, FrameId>
```

`DashMap` is sharded; concurrent inserts and lookups on different keys don't serialize.

## Fetch algorithm

### Cache hit
```
1. Look up page_id in page_table → frame_id (or miss)
2. If found:
   a. Lock frame.state briefly
   b. If frame.state.page_id != page_id: race lost; goto miss path
   c. Increment pin_count
   d. Saturating-increment usage_count (cap at 3)
   e. Release frame.state
   f. Acquire content_latch (read or write per caller intent)
   g. Return guard
3. If not found: goto miss path
```

### Cache miss
```
1. Acquire allocation latch (one global, but only held during eviction)
2. Re-check page_table (another thread may have loaded it)
3. If found now: drop latch, proceed as cache hit
4. Otherwise, find a victim via clock sweep:
   a. Walk frames starting at clock_hand
   b. For each frame:
      - lock frame.state
      - if pin_count > 0: skip
      - if usage_count > 0: decrement, release, continue
      - else: this is the victim
5. If victim frame is dirty:
   a. wal.flush_through(frame.state.page_lsn)
   b. disk.write_page(frame.state.page_id, frame.data)
   c. (no need to sync the heap file here; the WAL is durable already)
6. Remove old page_id from page_table (if frame was occupied)
7. Update frame.state: page_id = new, dirty = false, usage_count = 1, pin_count = 1
8. Insert new (page_id, frame_id) into page_table
9. disk.read_page(page_id, frame.data)
10. Release allocation latch
11. Acquire content_latch
12. Return guard
```

The allocation latch is a coarse lock held only for the clock walk and page table mutation; under heavy contention this can become a bottleneck. v2 may shard the buffer pool to reduce contention.

### Clock walk bound
In the worst case, every frame has `usage_count = 3` and every frame is pinned. The walk must decrement each frame three times before it can find a victim, then must wait for pins to be released. We bound the walk: after 4N frame visits without finding a victim, the fetch returns `BufferPoolExhausted`. The caller (typically the executor) treats this as an out-of-memory condition.

## Eviction and the WAL invariant

The invariant: **a dirty page cannot be written to disk until the WAL is durable through its page_lsn.**

Enforced at the only place dirty pages move to disk: the eviction path (step 5a above) and `flush_through` / `flush_all`. Each calls `wal.flush_through(page_lsn)` before `disk.write_page`. `wal.flush_through` is idempotent and cheap when the LSN is already durable; in the typical case it returns immediately.

This is one of the two most important correctness invariants in the engine. The other is the visibility logic in MVCC.

## Background page cleaner

A dedicated thread (started by `BufferPool::new`) periodically flushes dirty pages opportunistically:

```
loop:
    sleep(50ms)
    candidates = scan dirty page list (sorted by page_lsn ascending)
    for each candidate:
        if frame is unpinned and usage_count == 0:
            flush it
```

The cleaner reduces the work the eviction path has to do; under steady state, most evictions find clean victims.

## Dirty page tracking

Two ways to find dirty pages:
1. Walk every frame looking at `dirty`. Slow.
2. Maintain a `DashSet<PageId>` of dirty pages, updated when a frame transitions clean→dirty and dirty→clean.

We do (2). It costs a hash insert per transition (rare) and gives O(dirty pages) iteration for the cleaner and the checkpointer.

## Checkpoint integration

Fuzzy checkpoint (see `components/wal.md`) snapshots the dirty page table:
```
fn snapshot_dirty_pages(&self) -> Vec<(PageId, Lsn)>;
```
The checkpoint record contains this snapshot. On recovery, the analysis phase uses it to bound the redo start LSN.

## Pinning correctness

The most common pin bug is forgetting to unpin. We mitigate:

1. `PageReadGuard` / `PageWriteGuard` implement `Drop` to unpin.
2. Debug builds: a `PinTracker` records every pin with a stack trace; at shutdown, any non-zero pin counts produce a fatal log message identifying the unbalanced site.
3. The number of in-flight pins per thread is capped (e.g., 16). A thread holding too many pins is a likely bug.

Double-pin and pin-after-unpin are detected by the guard type system: guards are not `Copy`; only one owner at a time.

## Concurrency

- Frame state mutex: fine-grained, short-held.
- Content latch: per-frame `RwLock`. Multiple readers OR one writer.
- Page table: `DashMap`, internally sharded.
- Allocation latch: one global `Mutex`. Held during cache-miss handling. The bottleneck under thrashing workloads; acceptable for v1.

## Configuration

```toml
[buffer_pool]
size_mib = 1024              # total memory budget
clock_sweep_decrement = 1    # how much usage_count drops per pass
background_cleaner_interval_ms = 50
allocation_latch_timeout_ms = 5000
```

## Metrics

- `prism_buffer_hits_total`
- `prism_buffer_misses_total`
- `prism_buffer_evictions_total`
- `prism_buffer_dirty_writes_total`
- `prism_buffer_pin_count`
- `prism_buffer_fetch_latency_seconds`

## Testing

- Unit: every state transition.
- Property: random pins/unpins/fetches; verify pin counts settle at zero, no double-pin, no missing entries.
- Stress: 64 threads, random access pattern, verify no corruption and bounded latency.
- Fault injection: combined with disk manager's fault injector; verify WAL invariant holds under torn writes.

## References

- ADR 0007 — clock sweep choice.
- ADR 0003 — WAL invariant.
- `components/wal.md` — `flush_through` contract.
- PostgreSQL `bufmgr.c` — reference implementation.
