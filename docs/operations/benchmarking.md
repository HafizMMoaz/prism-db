# Operations: Benchmarking

**Status:** Accepted
**Last updated:** 2026-05-15

This document describes the benchmark suite, baseline targets, and how to interpret results. Benchmarks exist to (a) catch performance regressions, (b) inform optimization decisions, and (c) set realistic expectations for users.

## Tools

- **`criterion`** for microbenchmarks (function-level, per-crate).
- **`prism-bench`** binary for end-to-end workload benchmarks against a running server.
- **`flamegraph`** (via `cargo-flamegraph`) for profiling specific scenarios.

## Microbenchmarks

Each performance-sensitive crate has a `benches/` directory with criterion benchmarks. Examples:

- `prism-buffer/benches/fetch.rs`: page fetch latency, hit and miss.
- `prism-wal/benches/append.rs`: WAL append throughput, with and without group commit.
- `prism-index/benches/btree.rs`: B+tree search, insert, range scan latencies for varying sizes.
- `prism-core/benches/mvcc.rs`: visibility function on synthetic snapshots.

Run:

```bash
cargo bench -p prism-buffer
cargo bench -p prism-buffer -- fetch_hit         # one benchmark
```

Criterion produces HTML reports in `target/criterion/` with throughput, latency distributions, and comparisons against prior runs.

## Macrobenchmarks

`prism-bench` runs realistic workloads against a live `prismd` process.

```bash
# Start server
prismd run --data-dir /tmp/bench-data --config bench.toml

# Run workload
prism-bench tpcc \
  --warehouses 10 \
  --connections 50 \
  --duration 300s \
  --output report.json
```

### Workloads

#### TPC-C-like (SQL OLTP)
- Schema: warehouses, districts, customers, items, orders, order_lines, stock.
- Mix: 45% new-order, 43% payment, 4% order-status, 4% delivery, 4% stock-level.
- Reports tpmC (transactions per minute, NewOrder).

#### YCSB-A, B, C, D, F (KV)
- A: 50/50 read/write.
- B: 95/5.
- C: 100% read.
- D: read-latest (skewed toward recent writes).
- F: read-modify-write.
- Reports ops/sec and per-operation latency percentiles (p50, p99, p99.9).

#### Document workload (synthetic)
- 80% insertOne, 15% findOne by indexed field, 5% updateOne.
- Documents averaging 1 KiB with nested structure.
- Reports docs/sec and latency distribution.

#### Cross-model
- Mixed: each transaction touches all three models (insert SQL row, insert document, update KV counter).
- Validates that single-WAL design doesn't bottleneck under cross-model load.

#### Recovery time
- Workload, kill, restart, measure time to "ready".
- Reports recovery duration as a function of WAL bytes to replay.

## Baseline targets

These are the engineering targets, not promises. They are achievable on a modern server (8 cores, NVMe SSD, 32 GiB RAM, Linux). Reality may vary by hardware, workload, and configuration.

### Latency targets (p99, light load)

| Operation | Target |
|---|---|
| Single-row SELECT by PK | < 0.5 ms |
| Single-row INSERT (one table, no indexes) | < 2 ms |
| KV `get` (point) | < 0.3 ms |
| KV `put` | < 2 ms |
| Document `findOne` (indexed) | < 0.5 ms |
| Document `insertOne` | < 2 ms |
| Cross-model transaction (3 ops, commit) | < 5 ms |

The hard floor is the WAL fsync, which is typically 500 µs - 2 ms on consumer NVMe and 100-500 µs on enterprise NVMe. Group commit improves throughput but not minimum latency.

### Throughput targets

| Workload | Target |
|---|---|
| YCSB-C (100% read, in-memory) | 50,000 ops/sec |
| YCSB-A (50/50, in-memory) | 15,000 ops/sec |
| YCSB-A (50/50, 5x memory) | 5,000 ops/sec |
| TPC-C-like, 10 warehouses | 2,000 tpmC |
| Cross-model workload | 3,000 txn/sec |

These are deliberately modest. Postgres on the same hardware will be faster on most workloads; SQLite will be comparable for single-connection workloads. We are not trying to beat anything; we are building a correct system that performs reasonably.

### Recovery time target

Recovery from a 1 GiB WAL after a crash: < 60 seconds.

The dominant cost is sequential WAL read (throughput-bound) plus per-record CPU work. The 60-second target is intentionally generous; we expect to do better in practice.

## Measurement methodology

### Warm-up

Every benchmark warms up before measurement:
- Database loaded with the workload's initial dataset.
- 30 seconds of workload run before metrics collection starts (lets buffer pool warm and group-commit batching stabilize).
- Metrics collected over 5 minutes minimum.

### Variance

Each benchmark is run three times. Reported numbers are the median across runs; the range is reported alongside. Run-to-run variance > 10% is investigated.

### Hardware

The "official" benchmark hardware: c6i.4xlarge equivalent (16 vCPU, 32 GiB RAM, 1 TiB NVMe SSD). Smaller machines are used for development; tagged-release benchmarks are run on the official spec.

### Disabled features

Benchmarks document what is disabled. We do not silently enable things for benchmarks that we would not enable for users.

### Comparison points

For each workload, we publish results from:
- Prism (current commit).
- Postgres 16 (same hardware, default config + minimal tuning).
- SQLite 3 (where applicable; single-threaded only).
- Redis (for KV workloads only).

This is not a marketing exercise. The numbers are honest and the comparisons context the reader.

## Profiling

### CPU profiling

```bash
cargo flamegraph --bin prismd -- run --data-dir /tmp/data
# In another shell, drive load via prism-bench
# Stop prismd; flamegraph.svg is produced
```

### Memory profiling

```bash
RUSTFLAGS="-Z sanitizer=memory" cargo +nightly build
```

For allocation tracking:
```bash
cargo run --features="dhat-heap" ...
```

`dhat` reports allocation profiles. Used during the buffer pool and executor design to validate memory bounds.

### Latency profiling

The server emits histogram metrics for every internal latency we care about. Prometheus + Grafana for visualization; `bench-analyze` script for quick stats on a Prometheus snapshot.

## Performance regression CI

Nightly job:
1. Build the current `main`.
2. Run a fixed benchmark suite.
3. Compare against the 7-day rolling median.
4. If any metric regresses > 10% with high confidence (Mann-Whitney U test), the job fails and notifies the team.

False positives are common; the job's failure starts an investigation, not a rollback. But it is the trip-wire that has caught most performance regressions early.

## Adversarial benchmarks

In addition to the standard mix, we run benchmarks designed to expose worst-case behavior:

- Long version chains (1000 updates to one row).
- Deep B+tree (10M keys; the tree should be 4-5 levels).
- Skewed key distribution (Zipfian α=1.0).
- Largest allowable records.
- Mixed-size documents in one collection.

These are not the primary numbers, but they catch design flaws that the median workload would hide.

## Anti-goals

- We are not optimizing for benchmark numbers at the cost of correctness or simplicity.
- We are not chasing parity with hand-tuned production databases.
- We do not publish numbers without methodology.

## References

- `criterion` documentation: https://bheisler.github.io/criterion.rs/book/
- `cargo-flamegraph`: https://github.com/flamegraph-rs/flamegraph
- TPC-C specification: https://www.tpc.org/tpcc/
- YCSB: https://github.com/brianfrankcooper/YCSB
- `operations/testing-strategy.md` — broader test context.
