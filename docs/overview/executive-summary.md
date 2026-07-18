# Executive Summary

**Status:** Accepted
**Last updated:** 2026-05-15

## The problem

Modern application development pushes engineers toward polyglot persistence: Postgres for relational data, MongoDB for document storage, Redis for ephemeral key-value workloads. Each system is excellent within its model. Across models, applications inherit problems that the storage layer should solve:

- **No atomicity across stores.** Writing a user record to Postgres and a session token to Redis is two independent operations. Saga patterns and two-phase commit protocols paper over this, imperfectly, at the cost of latency and code complexity.
- **Three operational footprints.** Three sets of credentials, three backup pipelines, three monitoring surfaces, three sets of failure modes.
- **Three skill sets.** A team that fluently tunes Postgres often does not fluently tune Mongo, and almost never fluently tunes Redis at scale.
- **Three consistency models.** Postgres is serializable on demand; Mongo is per-document by default; Redis has its own semantics. Reasoning about an application that touches all three requires holding three models in your head simultaneously.

## The thesis

The storage primitive underneath all three models is the same: a tuple of bytes, written through a write-ahead log, addressable by a logical identifier, versioned for concurrent readers. The user-facing differences - tables with schemas, schemaless documents, opaque key-value pairs - are access methods, not storage methods.

If we commit to that observation in the design, one engine can serve all three models with a shared buffer pool, a shared WAL, and a shared transaction manager. A single transaction can touch any combination of the three. ACID is inherited; it does not need to be reconstructed at the application layer.

## What we are building

Prism is a single-node embedded-or-server database engine, written in Rust, providing:

1. **A unified storage layer.** 8 KiB slotted pages, one heap file per database, page-grained WAL with ARIES-style recovery.
2. **A unified transaction manager.** MVCC with snapshot isolation. One transaction ID space across all models. One commit record per transaction regardless of how many models it touched.
3. **Three access methods on top:**
   - **Relational** - SQL surface (subset of SQL:2016), B+tree primary and secondary indexes, Volcano executor.
   - **Document** - schemaless documents stored as tagged binary blobs, indexed by `_id` and by user-declared field paths.
   - **Key-value** - ordered keys with point and range access, hash index for point lookups, optional ordered index for ranges.
4. **Network and embedded access.** Binary TCP protocol for remote clients; in-process API for embedded use. Official SDK for Node.js via `napi-rs`. Interactive shell for ad-hoc work.

## Success criteria for v1.0

| Criterion | Target |
|---|---|
| Cross-model atomicity | A transaction inserting one row, one document, and one KV pair commits atomically or aborts atomically. Verified by linearizability tests. |
| Crash recovery | `kill -9` at any point during operation leaves the database in a consistent state on next start. Verified by randomized fault injection. |
| Snapshot isolation | No write skew detection (that is serializable). Read-only transactions never block. Verified by Elle-style anomaly checking. |
| Single-node throughput | 50,000 single-row inserts/sec on a c6i.xlarge equivalent, persistent, with `fsync` on commit. Indicative not guaranteed; baseline against SQLite WAL mode. |
| Recovery time | 1 GB database with 100 MB of active log replays in under 30 seconds. |
| API stability | SDK and shell APIs versioned. Breaking changes require a major version bump and a migration note. |

## What this document does not commit to

- A query optimizer beyond predicate pushdown and basic index selection.
- Distributed transactions, replication, or sharding.
- Column-store, vectorized execution, or analytical query support.
- Wire compatibility with Postgres, MongoDB, or Redis.
- A managed cloud offering.

Each of these is a credible follow-on project. None are in scope for v1.

## How to evaluate this proposal

The four highest-leverage decisions are documented in ADRs 0001, 0003, 0004, and 0006. If you disagree with those four, you disagree with the project; engage there first. The rest of the corpus follows.
