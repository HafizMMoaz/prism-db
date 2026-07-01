# Component: SQL Engine

**Crate:** `prism-sql`
**Status:** Accepted
**Last updated:** 2026-06-15

## Purpose

The SQL engine translates SQL strings into executor pipelines that operate on the record store. It is the relational access method. Its job is to be a clean, narrow, predictable implementation of a useful subset of SQL — not to compete with Postgres on feature breadth.

> **Design target vs. what ships today.** The pipeline, binder, rewriter,
> planner, and Volcano operators described below are the *target* architecture.
> The engine that ships today interprets the parsed `sqlparser-rs` AST directly
> against the catalog — there is no separate bind/rewrite/plan IR yet, joins are
> nested-loop, and index use is a rule-based equality seek (no cost model). The
> **[Implemented surface](#implemented-surface-current)** section below is the
> authoritative list of what actually works; the `prism-sql` crate-level doc
> comment tracks it line-for-line.

## Implemented surface (current)

What the shipping engine accepts today. This mirrors the `prism-sql` crate-level
doc comment, which is kept authoritative.

**DDL & DML**
- `CREATE TABLE` with constraints: `PRIMARY KEY`, `NOT NULL`, `UNIQUE`, literal
  `DEFAULT`, column- or table-level `CHECK (…)` (enforced on `INSERT`/`UPDATE`),
  and `FOREIGN KEY`/`REFERENCES` (child checked on `INSERT`/`UPDATE`, parent
  `RESTRICT` on `DELETE`). `ALTER TABLE` (add / drop / rename column, rename
  table), `DROP TABLE`.
- `CREATE [OR REPLACE] VIEW v AS <query>` / `DROP VIEW [IF EXISTS] v` — logical
  views. The view's `SELECT` text is stored in the catalog and expanded into a
  derived subquery wherever the view is referenced (so views may build on other
  views; a cyclic definition is caught by a depth limit). Materialized views and
  an explicit view column list (`CREATE VIEW v (a, b) AS …`) are deferred.
- `CREATE [UNIQUE] INDEX name ON t (c, …)` / `DROP INDEX` — secondary B+tree
  indexes over one **or more** columns, `UNIQUE` or non-unique. `UNIQUE` is
  enforced on `INSERT`/`UPDATE`; both can serve equality seeks.
- `INSERT … VALUES (…), …` and `INSERT … SELECT` (the source query materializes
  before any insert, so inserting from the same table is safe),
  `UPDATE t SET … [WHERE …]`, `DELETE FROM t [WHERE …]`. (Updating a
  `PRIMARY KEY` column is deferred.)

**Queries** — `SELECT [DISTINCT] <exprs | *> FROM … [WHERE …]
[GROUP BY … [HAVING …]] [ORDER BY … [ASC|DESC]] [LIMIT n] [OFFSET n]`, combinable
with `UNION` / `INTERSECT` / `EXCEPT` (each `ALL` or distinct; the outer
`ORDER BY`/`LIMIT`/`OFFSET` binds to the combined result).
- **Access path:** sequential scan, with a rule-based **index seek** when (within
  a top-level `AND`) the `WHERE` pins the primary key — or every column of a
  secondary index — to a literal, or bounds the primary key with a range
  (`>`/`>=`/`<`/`<=`/`BETWEEN`, fixed-width key types). The residual predicate is
  re-applied to the seeked rows.
- **Joins** (nested-loop): `INNER`, `LEFT`, `RIGHT`, `FULL OUTER`, `CROSS`,
  comma-separated cartesian products, and self-joins via aliases — with `ON`,
  `USING (…)`, and `NATURAL` (the latter two coalesce the join columns).
  `t.col`-qualified references work throughout the statement.
- **Aggregates:** `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, with optional
  `GROUP BY … HAVING …`. `ORDER BY` may reference aggregate output by name,
  1-based ordinal, or expression text.
- **Subqueries:** scalar `(SELECT …)`, `x [NOT] IN (SELECT …)`,
  `[NOT] EXISTS (SELECT …)`, and derived tables `FROM (SELECT …) AS a`.
  Uncorrelated subqueries run once up front; **`WHERE` subqueries may be
  correlated** (re-evaluated per outer row by decorrelation). Correlated
  subqueries *outside* `WHERE` (e.g. in the select list) are deferred.
- **CTEs:** non-recursive `WITH a AS (…) [, b AS (…)] …`, inlined as derived
  tables (recursive CTEs are deferred).
- **Window functions:** `ROW_NUMBER`/`RANK`/`DENSE_RANK`/`LAG`/`LEAD` and the
  aggregates `SUM`/`COUNT`/`AVG`/`MIN`/`MAX` over
  `OVER (PARTITION BY … ORDER BY …)`, one value per row. Aggregate windows cover
  the whole partition; explicit frame clauses (`ROWS`/`RANGE …`) are deferred.

**Expressions** — arithmetic (`+ - * / %`), comparisons, `AND`/`OR`/`NOT`,
`IS [NOT] NULL`, `[NOT] IN (…)`, `[NOT] BETWEEN … AND …`, `[NOT] LIKE` (`%`/`_`),
`CASE`, `CAST(x AS <type>)`, and scalar functions:
- **date/time:** `NOW`, `CURDATE`, `YEAR`/`MONTH`/`DAY`/`HOUR`/`MINUTE`/`SECOND`/
  `QUARTER`/`DAYOFWEEK`/`DAYOFYEAR`, `DATEDIFF`, `DATE_ADD`/`DATE_SUB` with
  `INTERVAL n DAY|HOUR|…`, `UNIX_TIMESTAMP`/`FROM_UNIXTIME`;
- **string:** `UPPER`/`LOWER`/`LENGTH`/`SUBSTR`/`TRIM`/`CONCAT`/`REPLACE`/`LEFT`/
  `RIGHT`/`REVERSE`/`REPEAT`/`SPACE`/`LPAD`/`RPAD`/`INSTR`/`LOCATE`/`ASCII`;
- **numeric:** `ABS`/`MOD`/`ROUND`/`CEIL`/`FLOOR`/`POW`/`SQRT`/`EXP`/`LN`/`LOG`/
  `LOG10`/`LOG2`/`SIGN`/`TRUNCATE`/`PI`/`GREATEST`/`LEAST`;
- **control flow:** `IF`/`IFNULL`/`NULLIF`/`COALESCE`.

**Deferred** (the design sections below describe the eventual home for these):
correlated subqueries outside `WHERE`, updating a primary-key column, join
predicate pushdown / index nested-loop joins, leading-prefix index seeks (and
range seeks on secondary or text-keyed indexes), and the formal
bind → rewrite → plan IR with cost-based planning.

## Pipeline

```
SQL string
  ↓ parse (sqlparser-rs)
AST
  ↓ bind (resolve against catalog)
Bound logical plan
  ↓ rewrite (predicate pushdown, projection prune, constant folding)
Optimized logical plan
  ↓ plan (choose physical operators, index selection)
Physical plan
  ↓ execute (Volcano operators)
Result tuples
```

## Parser

We use `sqlparser-rs` for the parser. It supports a broad SQL grammar; we accept a subset (see `vision-and-scope.md`) and reject the rest at the binder stage.

The parser produces an AST. We do not modify `sqlparser-rs` ASTs in place; the binder consumes them and produces our own bound representation.

## Binder

The binder:
1. Resolves identifiers against the catalog: table names → OIDs, column names → column OIDs.
2. Type-checks: every expression has a determined type; mismatches are errors.
3. Substitutes parameter placeholders (`$1`, `$2`) with their bound types.
4. Resolves stars (`SELECT *`) to column lists.
5. Validates: no aggregate-in-WHERE, no column-not-in-GROUP-BY-or-aggregate, no unknown function.

Output: a `BoundQuery` AST with every identifier resolved and every expression typed.

## Rewriter

Applies simple, deterministic rewrites:

- **Predicate pushdown:** Move `WHERE` predicates as close to the scan as possible. A predicate on a single table moves below the join.
- **Projection pruning:** Drop columns not used downstream.
- **Constant folding:** `WHERE x = 1 + 2` → `WHERE x = 3`.
- **Predicate normalization:** Conjunctive normal form for joins; helps the planner find equi-join keys.
- **Subquery flattening:** Convert simple `IN (SELECT ...)` and `EXISTS (SELECT ...)` to semi-joins where possible. Complex subqueries remain as correlated subplans.

We do not implement cost-based join reordering in v1. The order in the query is the order in execution. Users who care about join order order their queries thoughtfully.

## Planner

Chooses physical operators for each logical step:

- Scans:
  - If a `WHERE` predicate on an indexed column allows a key range → `IndexScan`.
  - Otherwise → `SeqScan`.
- Joins:
  - Equi-join with available memory → `HashJoin` (build on the smaller relation if known, else the right).
  - Otherwise → `NestedLoopJoin`.
- Aggregates:
  - Always hash-based aggregation.
- Sort:
  - In-memory sort up to the memory budget.

Index selection is rule-based, not cost-based. The rule: for each predicate, find indexes that match a column; choose the most selective available index based on the predicate's operator (equality > range > nothing).

This will sometimes pick suboptimally. We document the limitations and add cost-based selection in v2.

## Physical operators

Defined in ADR 0010. Each implements `PhysicalOperator { open, next, close }`. Operators are stateful; the planner instantiates them with their configuration (tables, predicates, indexes) and the executor invokes them.

### SeqScan
- `open`: opens a heap iterator at the first page of the table.
- `next`: returns the next visible tuple. Internally walks pages and slot directories. Calls `record_store.read` for visibility and version-chain handling.
- `close`: drops the iterator and releases any pinned pages.

### IndexScan
- `open`: positions the B+tree (or hash index) at the start of the requested range.
- `next`: returns the next `(key, rid)` and resolves rid → visible tuple. Skips invisibles.
- `close`: drops the iterator.

### HashJoin
- `open`:
  - Builds the hash table by exhausting the build side.
  - Each build tuple is hashed on the join key and inserted into the hash table.
- `next`:
  - Pulls the next probe tuple.
  - Looks up matching build tuples in the hash table.
  - Emits joined tuples; if multiple matches, emits one per match (state preserved across calls).
- `close`: drops the hash table.

### Aggregate
- `open`:
  - Exhausts the input.
  - Hashes each tuple on the GROUP BY key.
  - Updates accumulators (COUNT, SUM, etc.) in the corresponding group.
- `next`:
  - Yields each `(group_keys, aggregates)` row.
- `close`: drops the hash table.

### Sort
- `open`:
  - Buffers all input tuples.
  - Sorts using `slice::sort_by`.
- `next`: yields tuples in sorted order.
- `close`: drops the buffer.

In-memory sorting only. Inputs that exceed the memory budget abort with `OutOfMemory`.

### Insert
- `open`:
  - For an INSERT VALUES, materializes the values rows.
  - For INSERT SELECT, opens the inner subplan.
- `next`:
  - Pulls a row from input, serializes, calls `record_store.insert`, updates affected indexes.
  - Repeat until input exhausted.
  - Yields one row containing the affected count.
- `close`: closes the inner plan.

### Update / Delete
Analogous: pull input from a SeqScan or IndexScan, apply visibility, call `record_store.update` / `delete`, update indexes.

## Expression evaluation

Expressions in `WHERE`, `SELECT` projections, etc. are evaluated by an interpreter:

```rust
fn eval(expr: &Expr, row: &Row, ctx: &ExecCtx) -> Result<Value>;
```

Type checking happened at bind time. Evaluation produces `Value`, an enum over the supported types.

No JIT, no expression compilation. Pure tree-walking interpreter. Adequate for OLTP queries where the per-tuple cost is small relative to I/O.

## Type system

The implemented SQL column types and their runtime values (`prism-sql/src/types.rs`):

```rust
pub enum Type {
    Bool,         // BOOL / BOOLEAN
    Int64,        // BIGINT / INT / INTEGER
    Double,       // DOUBLE / FLOAT / REAL
    Timestamp,    // TIMESTAMP — epoch microseconds
    Text,         // TEXT / VARCHAR / CHAR
}

pub enum Value {
    Null,
    Bool(bool),
    Int64(i64),
    Double(f64),
    Timestamp(i64),    // microseconds since Unix epoch
    Text(Box<str>),
}
```

`Int64` is the one integer width and `Double` the one float width (narrower
SQL spellings map onto them). The binary **wire/SDK** value space is broader —
it also carries `Int32`, `Float32`, `Binary`, and `ObjectId` tags for the KV and
document models (`docs/specs/record-format.md`) — but a relational column is one
of the five types above.

Coercion: integers widen to doubles in mixed arithmetic; `TIMESTAMP` parses from
`'YYYY-MM-DD[ HH:MM:SS]'` text (and from epoch integers) on insert; otherwise
conversion between scalar types is explicit via `CAST(x AS <type>)`.

## Memory accounting

Each query has a memory budget (default 256 MiB). Operators that buffer (HashJoin build side, Aggregate hash table, Sort) report memory growth to a per-query accountant. Exceeding the budget aborts the query with `OutOfMemory`.

This prevents one runaway query from OOM-killing the server.

## Configuration

```toml
[sql]
default_query_memory_mib = 256
default_query_timeout_secs = 30
allow_implicit_cross_join = false
```

## Metrics

- `prism_sql_queries_total{type="select"|"insert"|"update"|"delete"|"ddl"}`
- `prism_sql_query_duration_seconds{type}` (histogram)
- `prism_sql_parse_errors_total`
- `prism_sql_plan_errors_total`
- `prism_sql_rows_examined_total`
- `prism_sql_rows_returned_total`

## Testing

- Unit: parser, binder, rewriter, planner, each operator.
- Property: random queries (constrained to the supported subset) over synthetic data; compare results to a reference implementation (SQLite as the oracle).
- Conformance: a subset of the SQL standard test suite, where the subset is the supported feature set.

## References

- ADR 0010 — Volcano execution.
- `components/catalog.md` — what the binder consults.
- `components/btree.md`, `components/mvcc.md` — what the executor calls.
- `sqlparser-rs`: https://github.com/sqlparser-rs/sqlparser-rs
- PostgreSQL's executor as a reference.
