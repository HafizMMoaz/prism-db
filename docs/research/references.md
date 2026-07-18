# Research: References

**Status:** Living document
**Last updated:** 2026-05-15

This is the reading list. Papers and books that informed Prism's design; prior-art systems we have studied; and resources for further depth. Curated, not exhaustive.

## Foundational papers

### Recovery

**Mohan, Haderle, Lindsay, Pirahesh, Schwarz. "ARIES: A Transaction Recovery Method Supporting Fine-Granularity Locking and Partial Rollbacks Using Write-Ahead Logging."** ACM TODS, March 1992.

The blueprint for our recovery. Three-phase recovery (analysis, redo, undo), physiological logging, CLRs, fuzzy checkpointing. Read it at least twice. The notation is dense but the algorithm description is precise.

**Mohan. "Repeating History Beyond ARIES."** VLDB 1999.

Mohan's retrospective. Useful for understanding why ARIES was designed the way it was and what later systems built on top.

### Concurrency control

**Bernstein, Hadzilacos, Goodman. *Concurrency Control and Recovery in Database Systems*.** Addison-Wesley, 1987.

The textbook. Out of print but available online from the authors. The chapters on serializability theory, locking, and timestamp ordering are foundational.

**Berenson, Bernstein, Gray, Melton, O'Neil, O'Neil. "A Critique of ANSI SQL Isolation Levels."** SIGMOD 1995.

Why the SQL standard's isolation levels are insufficient. Introduces snapshot isolation and the modern characterization of anomalies. Required reading before any discussion of isolation.

**Cahill, Röhm, Fekete. "Serializable Isolation for Snapshot Databases."** SIGMOD 2008.

How Postgres got serializable. The SSI algorithm. Out of scope for Prism v1 but documented here because v2 may revisit.

**Adya. "Weak Consistency: A Generalized Theory and Optimistic Implementations for Distributed Transactions."** PhD thesis, MIT, 1999.

The deepest treatment of isolation anomalies. Defines G0, G1, G2, etc. - the vocabulary Elle uses.

### Indexing

**Lehman, Yao. "Efficient Locking for Concurrent Operations on B-Trees."** ACM TODS, December 1981.

The B-link tree. Our B+tree concurrency model.

**Fagin, Nievergelt, Pippenger, Strong. "Extendible Hashing - A Fast Access Method for Dynamic Files."** ACM TODS, September 1979.

Our hash index.

**Graefe. "Modern B-Tree Techniques."** Foundations and Trends in Databases, 2011.

The comprehensive modern review. Variations, optimizations, lessons learned. Excellent reference.

### Storage and buffer management

**Effelsberg, Härder. "Principles of Database Buffer Management."** ACM TODS, December 1984.

Buffer pool design space. Steal/no-steal, force/no-force, the WAL invariant. Foundational.

**Stonebraker. "Operating System Support for Database Management."** CACM, July 1981.

Why OS file caches and database buffer pools should not coexist. The reason we use `O_DIRECT`.

### Query execution

**Graefe. "Volcano - An Extensible and Parallel Query Evaluation System."** TKDE, February 1994.

The Volcano model. The execution architecture we use.

**Graefe. "Query Evaluation Techniques for Large Databases."** ACM Computing Surveys, June 1993.

The companion survey. Operators, algorithms, parallelism. Comprehensive.

**Kersten, Leis, Kemper, Neumann, Stonebraker, Boncz. "Everything You Always Wanted to Know About Compiled and Vectorized Queries But Were Afraid to Ask."** VLDB 2018.

The honest comparison of execution models. Read before choosing between Volcano, vectorized, and compiled.

### MVCC

**Reed. "Naming and Synchronization in a Decentralized Computer System."** PhD thesis, MIT, 1978.

Origin of multiversion concurrency control.

**Bernstein, Goodman. "Multiversion Concurrency Control - Theory and Algorithms."** ACM TODS, December 1983.

The formal foundation.

### Distributed systems (background)

**Lamport. "Time, Clocks, and the Ordering of Events in a Distributed System."** CACM, July 1978.

Logical clocks, happens-before. Used here as background for understanding LSN-based ordering.

**Gilbert, Lynch. "Brewer's Conjecture and the Feasibility of Consistent, Available, Partition-Tolerant Web Services."** SIGACT News, June 2002.

CAP theorem. Background for why v1 is single-node.

## Books

**Gray, Reuter. *Transaction Processing: Concepts and Techniques*.** Morgan Kaufmann, 1992.

The bible. Out of print but findable. Encyclopedic treatment of every topic from locking to recovery to distributed two-phase commit.

**Hellerstein, Stonebraker, Hamilton. *Architecture of a Database System*.** Foundations and Trends in Databases, 2007.

The shorter, modern overview of database internals. Read this first if you're new to the area.

**Garcia-Molina, Ullman, Widom. *Database Systems: The Complete Book*.** Pearson, 2008.

Textbook. Solid coverage of everything.

**Petrov. *Database Internals*.** O'Reilly, 2019.

Modern, accessible introduction to storage engines and distributed databases. Less depth than the classics but easier to digest.

## Prior-art systems

### PostgreSQL

The reference implementation we consult most. Open source, well-documented, mature.

Useful starting points in the source:
- `src/backend/access/transam/xlog.c` - WAL.
- `src/backend/storage/buffer/bufmgr.c` - buffer pool.
- `src/backend/access/nbtree/` - B-tree.
- `src/backend/access/heap/heapam.c` - heap access methods.
- `src/backend/utils/time/snapmgr.c` - snapshot management.
- `src/backend/utils/cache/` - catalog cache.

The architecture differs from Prism in many ways - Postgres has full HOT, vacuum, replication, partitioning - but the fundamentals are recognizably similar.

### InnoDB (MySQL)

The other mature OLTP engine. Uses LSN, undo logs, doublewrite buffer (alternative to full-page-images), and a different MVCC implementation (older versions live in the undo log, not inline).

Worth studying for: doublewrite buffer as torn-write defense, clustered indexes (vs. Postgres's heap-organized tables), lock semantics.

### SQLite

Single-file, single-process, no concurrency for writers. Different model entirely, but extremely well-engineered. Worth reading the source: small enough to comprehend in a weekend.

Particularly the WAL mode and the rollback journal - two different recovery approaches in one codebase.

### TiKV

Distributed transactional key-value store written in Rust. Built on RocksDB. The transaction layer (Percolator-based) is interesting; the use of Rust at scale is informative for engineering practices.

### CockroachDB

Distributed SQL on top of a Raft-replicated KV store. Their blog has many deep posts on isolation, retries, and transaction layer design.

### Materialize

Streaming SQL in Rust. The query language and execution differ entirely from us, but their engineering culture (publications, blog posts, openness about tradeoffs) is what we aspire to.

### sled

Open-source embedded database in Rust. We do not depend on it; we study it.

### LMDB

The simplest production-grade database, copy-on-write semantics, single-file. The opposite design point from ours. Excellent for understanding the design space.

## Talks

**Stonebraker. "Performance Comparison of Vertica's Algorithms vs Volcano-Style Algorithms."** A canonical statement of why vectorization is better for analytical queries.

**Pavlo. "Self-Driving Databases." CMU lectures.** Andy Pavlo's CMU 15-721 course (advanced database systems) is recorded and free; it's the best public coverage of modern engine internals. Watch the videos on storage, transactions, and execution.

**Kingsbury (Jepsen). Various talks.** Aphyr's talks on testing distributed systems are required watching for anyone building one. The same techniques apply to single-node systems with crash recovery.

## Code

**rocksdb.** C++. The dominant LSM-tree storage engine. Different from us (LSM vs. B+tree), but the engineering is top-tier.

**foundationdb.** C++. Distributed transactional store with strong testing culture. Their simulation framework is the gold standard for deterministic testing.

**rustls.** Rust. TLS implementation we use. Excellent code.

**tokio.** Rust. Async runtime we use.

**tracing.** Rust. Structured logging library we use.

## Standards

**SQL:2016** and **SQL:2023.** ISO/IEC 9075. The current SQL standard. We implement a tiny subset; the standard is the reference for what each feature should mean.

**BSON spec.** https://bsonspec.org. Our document format is similar in spirit.

**RFC 793 (TCP), RFC 8446 (TLS 1.3).** Background for the network layer.

## Online resources

- **Andy Pavlo's CMU 15-445 / 15-721 courses:** https://15445.courses.cs.cmu.edu and https://15721.courses.cs.cmu.edu. Slides, assignments, videos. The single best free resource on database internals.
- **PostgreSQL wiki:** https://wiki.postgresql.org. Internals notes by the Postgres developers.
- **Jepsen analyses:** https://jepsen.io/analyses. Each one is a case study in how production databases fail and why.

## Reading order recommendation

For a new contributor:

1. Hellerstein/Stonebraker survey (1 day).
2. Berenson et al. on isolation (1 day).
3. ARIES paper (3 days; reread).
4. Lehman-Yao (1 day).
5. Petrov's *Database Internals* (1-2 weeks, ongoing reference).
6. Postgres source for relevant components, as needed.

After that, the rest is depth-as-needed.

## References to specific decisions

When ADRs cite papers, the citations are in this file. The intent is that anyone reading an ADR can come here for the full reference.

- ADR 0001 (Rust): no paper; engineering choice.
- ADR 0002 (page-based): Gray & Reuter; Petrov.
- ADR 0003 (ARIES): Mohan et al. 1992.
- ADR 0004 (MVCC/SI): Berenson et al.; Bernstein & Goodman 1983.
- ADR 0005 (record format): Gray & Reuter (record layout discussion).
- ADR 0006 (single WAL): Mohan; Gray & Reuter.
- ADR 0007 (clock sweep): Effelsberg & Härder.
- ADR 0008 (binary protocol): no paper.
- ADR 0009 (napi-rs): no paper.
- ADR 0010 (Volcano): Graefe 1994; Kersten et al. 2018.

## Closing note

A database engine is in some sense a four-decade conversation. The papers above are the high points. Nothing in Prism is original; what we've done is pick a coherent set of well-understood techniques and implement them carefully. That is the most we can hope for. The point of this list is to enable anyone working on Prism to participate in the conversation, not just consume its conclusions.
