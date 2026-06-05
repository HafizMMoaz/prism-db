# Roadmap

**Status:** Accepted
**Last updated:** 2026-05-15

This document describes the phased delivery plan for Prism v1.0. Dates are illustrative for a four-month engineering effort and will be re-baselined when implementation starts. The phase ordering is normative; the dates are not.

## Phase 0 — Design (current phase)

Lock the design before code begins. Output of this phase is the documentation corpus in this repository.

Exit criteria:
- All ADRs in `docs/adr/` are status `Accepted`.
- All component design docs exist in skeleton form.
- All specifications (`docs/specs/`) are complete enough to implement against.
- The risk register identifies known risks with owners.

## Phase 1 — Foundation

The plumbing nothing else can be built on top of. If Phase 1 is wrong, everything after it is wasted.

Deliverables:
- Disk manager: heap file abstraction, page read/write, sync semantics
- Page format: slotted page implementation, tested with property-based tests
- Buffer pool: clock-sweep replacement, pin/unpin, page table
- WAL: append-only log, LSN allocation, group commit, fsync
- A test harness that can `kill -9` the process at any LSN and verify recoverability

Exit criteria:
- 10,000 randomized fault-injection runs pass with no corruption.
- Buffer pool sustains target throughput for cached workloads (baseline TBD).
- WAL achieves target write throughput under group commit (baseline TBD).

## Phase 2 — Transactions and recovery

The conceptual heart of the engine. The phase most likely to slip.

Deliverables:
- Transaction manager: txn ID allocation, active txn table, commit log
- MVCC tuple format: xmin/xmax in tuple header, version chain pointers
- Visibility logic: snapshot semantics for reads, write conflict detection
- ARIES recovery: analysis, redo, undo phases with CLR support
- Fuzzy checkpointing
- Lock manager: per-tuple write locks, wait-for graph, deadlock detection

Exit criteria:
- ACID single-model transactions through a raw `(RecordId, bytes)` interface
- Recovery passes the Phase 1 fault-injection harness extended to multi-transaction workloads
- Concurrent stress test: N threads, random ops, random `kill -9`, all invariants hold

## Phase 3 — Access methods

Three layers on top of the unified store. Each is a thin layer once the foundation works.

Deliverables in order:
1. **Key-value engine.** Hash index over the record store. Simplest of the three, ships first as a smoke test.
2. **Catalog.** System tables describing user objects. Bootstrapped on database create.
3. **B+tree index.** Lehman-Yao variant for concurrent access. Used by both SQL and document indexes.
4. **Relational engine.** SQL parser (sqlparser-rs), Volcano executor, no optimizer beyond predicate pushdown.
5. **Document engine.** Tagged binary format, predicate compiler, path-based indexes.
6. **Cross-model transactions.** Already enabled by the unified transaction manager; this is verification, not new development.

Exit criteria:
- All three CRUD surfaces functional via in-process API
- Cross-model atomicity verified: insert row + update doc + put KV in one transaction, kill at any point, recovery is atomic
- Index usage verified by query plan dumps

## Phase 4 — Surface area

Make the engine usable from outside.

Deliverables:
- Network protocol server (TCP, binary, length-prefixed framing)
- Authentication and TLS
- Node.js SDK via napi-rs
- Interactive shell with multi-line input and per-model output formatting
- Structured logging and Prometheus metrics

Exit criteria:
- SDK published to a private npm registry
- Shell can connect, authenticate, run queries across all three models
- Metrics endpoint exposes core counters and histograms

## Phase 5 — Hardening

The phase that distinguishes "we built a database" from "we built a database that works."

Deliverables:
- Jepsen-style harness with Elle anomaly checker
- 24-hour soak test under random faults
- Online backup via WAL archiving
- Point-in-time recovery tool
- `prism-fsck` integrity checker
- Documentation completeness audit

Exit criteria:
- 24-hour soak: zero anomalies
- All v1.0 success criteria from the executive summary met
- Documentation describes every feature; binary contains no feature not documented

## Beyond v1.0

Not committed, ordered by likelihood of being the next major thing:

1. Replication (primary-backup, async)
2. Cost-based query optimizer
3. Serializable isolation (SSI on top of snapshot)
4. Vectorized execution for analytical queries
5. Distributed transactions
6. Multi-column and partial indexes

Each of these is a project on the order of one to two engineer-quarters. Sequencing depends on what the v1 users actually need.

## Dependencies and parallel work

Phases are sequential at the milestone boundaries. Within a phase, work can parallelize:

- Phase 1: disk + page format can proceed in parallel with WAL design; converge at buffer pool.
- Phase 2: recovery and transaction manager are interleaved; lock manager is independent and can run in parallel with both.
- Phase 3: KV, catalog, and B+tree are independent. SQL and document layers serialize behind B+tree.
- Phase 4: protocol, SDK, and shell are mostly independent.
- Phase 5: hardening is one workstream; documentation polish runs in parallel.
