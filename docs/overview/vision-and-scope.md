# Vision and Scope

**Status:** Accepted
**Last updated:** 2026-05-15

## Vision

Prism exists to remove the impedance mismatch between applications that need multiple data shapes and the storage infrastructure underneath them. The success condition for the project is that an engineering team building a system that today would use Postgres + Mongo + Redis can use Prism instead and lose nothing they relied on, while gaining cross-model ACID and a single operational footprint.

That phrasing is deliberate. We are not promising to be a better SQL engine than Postgres. We are not promising to be a better document engine than Mongo. We are promising to be a sufficient SQL engine, a sufficient document engine, and a sufficient KV engine, simultaneously, with one transaction manager.

"Sufficient" is the load-bearing word. We will not match Postgres on the long tail of SQL features. We will not match Mongo on aggregation pipelines. We will not match Redis on lua scripting or pub/sub. We will match all three on the operations that the 90th-percentile application uses 99% of the time, and we will do so with ACID guarantees that no combination of those three systems can provide.

## In scope for v1.0

### Storage and durability
- Single-node, single-database-file engine
- Page-based storage, 8 KiB pages, slotted page layout
- Write-ahead log with physiological logging, ARIES recovery
- Fuzzy checkpointing
- `fsync` on commit (configurable group commit)
- Crash recovery verified by randomized fault injection

### Transactions
- MVCC with snapshot isolation
- One transaction ID space, one commit log across all models
- Read-only transactions are wait-free for readers
- Deadlock detection via wait-for graph
- Per-tuple locking for writers
- Explicit `BEGIN` / `COMMIT` / `ABORT`; implicit single-statement transactions

### Relational access method
- SQL surface: `CREATE`/`ALTER`/`DROP TABLE`, `CREATE [OR REPLACE] VIEW`/`DROP VIEW` (logical), `CREATE [UNIQUE] INDEX`/`DROP INDEX`, `INSERT` (`VALUES` or `SELECT`), `UPDATE`, `DELETE`, `SELECT`
- Joins: inner, left, right, full outer, cross, and self-joins, with `ON`/`USING`/`NATURAL` (executor: nested-loop today; hash join is a target)
- `WHERE` predicates, `GROUP BY … HAVING`, `ORDER BY`, `LIMIT`, `OFFSET`, `DISTINCT`, and `UNION`/`INTERSECT`/`EXCEPT`
- Aggregates: `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`
- Non-recursive CTEs (`WITH … AS (…)`) and window functions (`ROW_NUMBER`/`RANK`/`DENSE_RANK`/`LAG`/`LEAD` and aggregate windows over `OVER (PARTITION BY … ORDER BY …)`)
- Subqueries (scalar, `IN`, `EXISTS`; correlated in `WHERE`), `CASE`, `CAST`, and date/string/numeric scalar functions
- B+tree primary key, single- and multi-column B+tree secondary indexes (`UNIQUE` and non-unique)
- Type system: `INT`, `BIGINT`, `FLOAT`, `DOUBLE`, `TEXT`, `BLOB`, `TIMESTAMP`, `BOOL`
- Constraints: `NOT NULL`, `UNIQUE`, `PRIMARY KEY`, literal `DEFAULT`, `CHECK`, and `FOREIGN KEY` (child checked on write, parent `RESTRICT` on delete)

### Document access method
- Collection-based, schemaless
- Documents stored as tagged binary (custom format, see `specs/record-format.md`)
- CRUD: `insertOne`, `insertMany`, `find`, `findOne`, `updateOne`, `updateMany`, `deleteOne`, `deleteMany`
- Predicate language: subset of MongoDB query operators (`$eq`, `$ne`, `$gt`, `$lt`, `$gte`, `$lte`, `$in`, `$nin`, `$and`, `$or`, `$exists`)
- Index on `_id` (always), index on user-declared field paths

### Key-value access method
- Namespace-based, byte-string keys, byte-string values
- Operations: `get`, `put`, `delete`, `range`, `scan`
- Hash index for point lookups (default)
- Ordered B+tree index for range queries (opt-in per namespace)

### Network and embedded
- TCP server with length-prefixed binary protocol
- Authentication: password-based, scrypt-hashed credentials
- TLS optional (configured at server startup)
- In-process embedded mode (link Prism as a library, no network)
- Node.js SDK via `napi-rs`, TypeScript definitions auto-generated
- Interactive shell with readline, multi-line input, output formatting per model

### Operability
- Structured JSON logging
- Prometheus metrics endpoint
- Online backup via WAL archiving
- Point-in-time recovery to any LSN
- Database file integrity check tool (`prism-fsck`)

## Out of scope for v1.0

These are explicitly excluded. Anyone arguing to pull them in is arguing to slip the schedule; that is a legitimate argument but it must be made explicitly.

### Distributed systems
- Replication (primary/replica or multi-master)
- Sharding
- Distributed transactions across nodes
- Consensus protocols (Raft, Paxos)
- Cross-region anything

### Advanced query
- Cost-based query optimizer (statistics-driven join reordering)
- Vectorized execution
- Columnar storage
- Materialized views (logical/non-materialized views **are** in scope - see above)
- Stored procedures or user-defined functions
- Recursive CTEs and window frame clauses (`ROWS`/`RANGE`) - non-recursive CTEs and unframed window functions **are** in scope
- Full-text search (beyond basic substring `LIKE`)

### Advanced indexing
- Partial indexes
- Functional/expression indexes
- Geospatial indexes
- Inverted indexes for full-text
- Bitmap indexes

### Advanced data types
- Arrays (beyond `BLOB`)
- User-defined types
- `JSON` type in SQL tables (use the document model instead)
- Spatial types

### Advanced isolation
- Serializable isolation (snapshot only in v1)
- Read-uncommitted, read-committed, repeatable-read as distinct levels (snapshot subsumes the useful ones)

### Ecosystem
- Wire compatibility with Postgres, MongoDB, Redis
- ODBC/JDBC drivers
- ORM integrations
- Cloud-managed service
- Web admin UI

## Constraints and assumptions

1. **Hardware.** We assume modern SSDs with `O_DIRECT` capability or equivalent. We do not optimize for spinning disks. We assume at least 4 GiB of RAM available for the buffer pool in production deployments.
2. **Operating system.** Linux, macOS, and Windows are all first-class, supported targets for development and production. The engine ships on all three. Platform-specific I/O (direct I/O, durable `fsync`, file locking) is abstracted behind a storage trait with a per-OS implementation and a portable buffered fallback; see `components/disk-manager.md`.
3. **Single process.** One Prism server process per database file. Multiple processes sharing one database file is unsupported.
4. **Workload.** OLTP. Transactions touching tens to thousands of records, not millions. Read-mostly workloads with point lookups and small range scans dominate the optimization target.

## Success conditions for declaring v1.0 done

All of the following must hold:

1. The success-criteria table in the executive summary is met or beaten.
2. The Jepsen-style test harness (see `operations/fault-injection.md`) runs for 24 hours of randomized workload with `kill -9` injected every 30-90 seconds and reports zero anomalies.
3. The SDK passes its own test suite on Linux x86_64, Linux aarch64, and macOS aarch64.
4. The documentation in this repository describes every feature shipped in the binary, and every feature in the binary appears in the documentation.
5. A user with no prior Prism experience can install the engine, write a small application against the SDK, and have it running in under thirty minutes, using only published documentation.

## Failure conditions

The project is in trouble if:

- Recovery becomes the bottleneck on commit latency. Recovery is correctness, not performance; the WAL design should never have to compromise correctness for throughput.
- Cross-model transactions require a separate code path from single-model transactions. The whole thesis fails if cross-model becomes a special case.
- The SDK lags the engine. The Node.js SDK must ship with every engine release; otherwise the surface that most users touch is broken.
