# ADR 0010: Volcano iterator execution model

**Status:** Accepted
**Date:** 2026-05-15

## Context

Once the SQL parser has produced a physical plan, something must execute it. The dominant execution models are:

1. **Volcano (Graefe 1994).** Operators implement `open() / next() / close()`. Each call to `next()` produces one tuple. Operators pull tuples from their children. Used by Postgres, MySQL (historically), SQLite, most older systems.

2. **Vectorized (MonetDB/X100, DuckDB).** Operators process batches of tuples (typically 1024 rows) at a time. Better CPU cache and branch prediction behavior. The standard for analytical engines.

3. **Push-based / compiled (HyPer, ClickHouse for parts, Umbra).** A whole-stage code generator emits a tight loop that pushes tuples from sources through a pipeline. Best raw performance, highest implementation complexity.

4. **Morsel-driven parallelism.** Combines vectorization with work-stealing parallelism. Production-grade analytical engines.

## Decision

Prism uses the **Volcano iterator model** for v1.0.

Each physical operator implements:
```rust
trait PhysicalOperator {
    fn open(&mut self, ctx: &ExecCtx) -> Result<()>;
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Tuple>>;
    fn close(&mut self, ctx: &ExecCtx) -> Result<()>;
}
```

Operators include: `SeqScan`, `IndexScan`, `Filter`, `Project`, `NestedLoopJoin`, `HashJoin`, `Aggregate`, `Sort`, `Limit`, `Insert`, `Update`, `Delete`.

## Alternatives considered

### Vectorized
**For:** Substantial CPU efficiency gain for scans, filters, and arithmetic. Mature ecosystem (DuckDB, Velox).

**Against:** Vectorization wins on analytical workloads where each operator touches many rows. OLTP queries touch tens to thousands of rows total; the per-batch overhead exceeds the per-row savings. Vectorized executors are also more complex to implement, especially for variable-length data (strings, documents).

If we were building an analytical engine, vectorization would be the right answer. We are not.

### Compiled / push-based
**For:** Best peak performance. Eliminates virtual dispatch overhead between operators.

**Against:** Substantial implementation complexity. The whole-stage compiler is a project on its own. JIT-compiled plans add a code-generation toolchain (LLVM, Cranelift) to the dependency set. Operator additions become harder. The team that has shipped a working code-generating executor in 4 months from a standing start does not exist.

### Morsel-driven
**For:** Excellent scaling on modern multicore hardware for analytical queries.

**Against:** Same reasoning as vectorized — overkill for OLTP queries that don't have enough work to parallelize within a single query.

## Why Volcano for Prism specifically

1. **Right-sized for OLTP.** Most Prism queries return small result sets. Tuple-at-a-time overhead is amortized over query parsing and network round-trips, both of which dominate. Vectorization's per-tuple savings are real but not where the latency lives.

2. **Simple to implement and reason about.** Each operator is independent. Testing operators in isolation is straightforward. A new join algorithm is a new struct implementing the trait; no codegen, no batch handling, no pipeline reorganization.

3. **Pull-based composes naturally with `LIMIT`.** A `LIMIT 10` operator pulls 10 tuples from its child and stops. No work wasted. Vectorized engines also handle this, but Volcano makes it trivial.

4. **Iterators in Rust are ergonomic.** `Iterator<Item = Result<Tuple>>` maps to Volcano's `next()` exactly. We can use Rust's iterator combinators for some operators (filter, take) and hand-write the ones that need state (joins, aggregates).

## What this rules out

Workloads that read or transform billions of rows in a single query will be measurably slower than they would be in a vectorized engine. We document this and direct analytical workloads elsewhere. It is not the workload we are building for.

## Execution context

```rust
struct ExecCtx<'a> {
    txn: &'a Transaction,
    record_store: &'a RecordStore,
    catalog: &'a Catalog,
    bind_params: &'a [Value],
    cancel_token: &'a CancellationToken,
}
```

Cancellation is cooperative: every operator's `next()` checks the token at the start and returns `Err(Cancelled)` if set. This is how query timeouts and connection drops propagate.

## Operator catalog (v1.0)

| Operator | Description |
|---|---|
| `SeqScan(table_oid)` | Heap scan over a table; yields visible tuples |
| `IndexScan(idx, range)` | Index seek + record fetch; respects MVCC |
| `Filter(predicate)` | Drops tuples failing predicate |
| `Project(exprs)` | Computes output columns |
| `NestedLoopJoin(predicate)` | Pulls from left, scans right per left tuple |
| `HashJoin(predicate, build_side)` | Builds hash table on build side, probes from other |
| `Aggregate(group_by, aggs)` | Hash-based grouping; supports COUNT, SUM, AVG, MIN, MAX |
| `Sort(keys)` | In-memory sort (external sort is a v2 enhancement) |
| `Limit(n, offset)` | Skips offset, returns up to n |
| `Insert(table_oid, indexes)` | Materialized child rows; writes to heap and indexes |
| `Update(table_oid, set_exprs)` | Updates visible tuples; produces new versions |
| `Delete(table_oid)` | Marks visible tuples deleted (sets xmax) |
| `Values(rows)` | Yields a literal list of rows |

## Memory budget

In v1, all sort and hash-join state is in memory. The query is aborted with `OUT_OF_MEMORY` if state grows beyond a configured limit (default 256 MiB per query). External sort and spillable hash join are post-v1.

## Consequences

### Enabled
- Simple operator framework, fast to implement and test.
- Natural fit for transactional workloads with small result sets.
- Cancellation and timeout via cooperative checks.
- Operator independence: new operators added without touching others.

### Constrained
- No vectorization; per-tuple virtual dispatch overhead is present (~5 ns per next() call). For million-row scans, measurable.
- No automatic parallelization within a query. Multi-query parallelism comes from multiple connections.
- In-memory operator state only; spillable state is post-v1.

### Required follow-on
- Operator implementations and their invariants → `components/sql-engine.md`.
- Memory accounting → `components/sql-engine.md`.

## References

- Graefe: "Volcano — An Extensible and Parallel Query Evaluation System." TKDE 1994.
- PostgreSQL executor source (`src/backend/executor/`).
- Kersten et al.: "Everything You Always Wanted to Know About Compiled and Vectorized Queries But Were Afraid to Ask." VLDB 2018. The reference comparison.
- ADR 0001 — Rust trait objects make Volcano operators clean to express.
