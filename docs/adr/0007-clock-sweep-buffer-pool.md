# ADR 0007: Clock-sweep buffer pool replacement

**Status:** Accepted
**Date:** 2026-05-15

## Context

The buffer pool decides which pages stay in memory under pressure. The replacement policy directly affects cache hit rate, which directly affects the performance of every read operation.

Candidate policies:

1. **LRU (Least Recently Used).** Doubly-linked list. Move accessed pages to head; evict from tail. Theoretically optimal under stable workloads; expensive under contention because every access mutates the global list.

2. **LRU-K.** Track the last K access times; evict the page with the oldest K-th-most-recent access. Better resistance to one-shot scans than LRU. More state per frame, more complex.

3. **Clock sweep (a.k.a. second-chance).** Each frame has a reference bit. Eviction walks frames in a circular order; if the bit is set, clear it and continue; if clear, evict. Approximates LRU at much lower cost.

4. **CLOCK-Pro / ARC.** More sophisticated adaptive policies. Used in some research systems.

5. **2Q.** Two queues for hot and cold pages.

PostgreSQL uses clock sweep. SQL Server uses a clock variant. InnoDB uses a midpoint-insertion LRU. RocksDB uses LRU but for block cache (not strictly a buffer pool). All major engines use approximations to LRU; none use strict LRU because the contention cost is prohibitive.

## Decision

Prism uses **clock sweep** for v1.0.

Each frame has a `usage_count` (0 to 3). On access, increment (capped at 3). The clock hand walks frames; on each frame, decrement; if the count reaches zero and the frame is unpinned and clean, evict.

## Alternatives considered

### Strict LRU
**For:** Theoretically best under predictable workloads.

**Against:** Every page access requires mutating the LRU list under a lock. With a buffer pool of 100,000 frames and thousands of concurrent operations per second, this lock becomes the bottleneck. The replacement quality is barely better than clock sweep in practice. The Postgres developers walked through this analysis in 2005 and chose clock; the same reasoning applies here.

### LRU-K
**For:** Better cache behavior under workloads with occasional large scans.

**Against:** More state per frame (multiple timestamps). More complex eviction logic. The win over clock sweep is real but small for OLTP workloads. We can add LRU-K behavior as a v2 enhancement if we observe scan pollution in benchmarks.

### CLOCK-Pro / ARC
**For:** Adaptive policies that handle workload changes well.

**Against:** ARC is patented (IBM). CLOCK-Pro is public but adds substantial implementation complexity for marginal wins on typical OLTP workloads. We are not in the buffer-replacement-research business.

### 2Q
**For:** Decent resistance to scan pollution.

**Against:** More moving parts than clock sweep without a clear win for our target workload. Possible v2 option.

## Why clock sweep specifically

1. **Low contention.** The clock hand and per-frame usage counts are updated under fine-grained latches. No global LRU list to serialize on.

2. **Acceptable replacement quality.** In practice, clock sweep is within a few percent of LRU on typical workloads. The hit-rate gap is much smaller than the cost gap.

3. **Simple to implement.** A circular index, a usage count per frame, a sweep loop. Roughly 300 lines of code total.

4. **Industry precedent.** Postgres has used this for two decades on workloads from tiny to enormous. The design is well-understood.

## Mechanics

```rust
struct Frame {
    page: Box<Page>,           // 8 KiB
    page_id: AtomicU64,        // current page (or sentinel for empty)
    pin_count: AtomicU32,      // 0 == evictable
    usage_count: AtomicU8,     // 0..=3, decremented by clock, incremented by access
    dirty: AtomicBool,
    latch: parking_lot::RwLock<()>,  // page content latch
}

struct BufferPool {
    frames: Vec<Frame>,        // fixed size at startup
    page_table: DashMap<PageId, FrameId>,
    clock_hand: AtomicUsize,
}
```

### Access path (cache hit)
1. Look up `page_id` in `page_table` → `frame_id`.
2. Pin frame (`pin_count.fetch_add(1)`).
3. Re-check that frame still holds the right `page_id` (could have been evicted between lookup and pin). If not, unpin and retry.
4. Increment `usage_count` (saturating at 3).
5. Acquire content latch (read or write).
6. Return page reference.

### Eviction path (cache miss)
1. Look up — miss.
2. Walk the clock hand:
   - For each frame: if pinned, skip. If unpinned and `usage_count > 0`, decrement and continue. If unpinned and `usage_count == 0`:
     - If dirty: enforce WAL invariant (`wal.flush_through(page.page_lsn)`), then write page to disk.
     - Remove old `page_id` from `page_table`.
     - This frame is now the victim.
3. Load the requested page into the victim frame.
4. Insert into `page_table`.
5. Return.

The walk is bounded: in the worst case (every frame has usage_count = 3 and all pins are held), the walk must scan 4N frames before finding a victim. In practice, the walk terminates quickly.

### Pin and unpin

```
fn pin(frame: &Frame) -> Result<PageGuard>;
fn unpin(frame: &Frame, dirty: bool);
```

`PageGuard` is RAII: drop unpins. Forgetting to unpin is a pin leak; debug builds assert pin_count == 0 at shutdown for every frame.

### Dirty page list

A separate concurrent set tracks dirty page IDs. The background page cleaner walks this set, flushes pages, and removes them from the set. Cleaning is opportunistic; under pressure, the eviction path also flushes.

## Sizing

Buffer pool size is configured at startup. Default: 25% of system RAM, capped at 16 GiB for safety. Operator-configurable via TOML.

Number of frames = `buffer_pool_bytes / page_size`. For an 8 KiB page and a 1 GiB pool, that's 131,072 frames.

## Consequences

### Enabled
- Low-contention page access via per-frame latches.
- Acceptable cache hit rates on typical workloads.
- Simple, well-understood implementation.

### Constrained
- Worst-case eviction scan is 4N frames. Pathological — never observed in practice — but bounded.
- No automatic adaptation to scan-heavy workloads. Mitigation: a `BUFFER_POOL_HINT_SCAN` flag on operations the SQL planner knows are scans, causing those frames to enter at `usage_count = 0` rather than 1. (Postgres's "ring buffer" trick, simplified.)

### Required follow-on
- Pin count overflow detection (we use u32; a billion concurrent pins of one frame would overflow, which is implausible but we assert it).
- Page table implementation choice (`DashMap` vs. sharded `HashMap` vs. custom). Benchmarks decide.

## References

- PostgreSQL `bufmgr` source: `src/backend/storage/buffer/bufmgr.c`.
- Effelsberg and Härder: "Principles of Database Buffer Management." TODS 1984. Survey of policies.
- O'Neil, O'Neil, Weikum: "The LRU-K Page Replacement Algorithm." SIGMOD 1993.
- ADR 0002 — page-based storage; the buffer pool exists to cache pages.
- `components/buffer-pool.md` — operational details.
