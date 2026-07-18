# Project: Milestones

**Status:** Accepted
**Last updated:** 2026-05-15

This document lists the milestones for v1.0, with target dates, owners (TBD), exit criteria, and dependencies. It is the project plan; the roadmap document (`overview/roadmap.md`) gives the narrative; this gives the gates.

## Milestone summary

| ID | Name | Target | Status |
|---|---|---|---|
| M0 | Design lock | 2026-05-15 | Active |
| M1 | Foundation | 2026-06-30 | Pending |
| M2 | Transactions and recovery | 2026-07-31 | Pending |
| M3 | Three models | 2026-09-15 | Pending |
| M4 | Surface and harness | 2026-10-15 | Pending |

## M0 - Design lock (2026-05-15)

**Goal:** All architectural decisions documented and accepted.

**Exit criteria:**
- All ADRs (0001-0010) marked Accepted.
- All component design docs written.
- All on-disk specifications written.
- Wire protocol and SDK API specified.

**Status:** Complete with this commit.

**Not in scope:** any code beyond a workspace skeleton and CI plumbing.

## M1 - Foundation (target 2026-06-30)

**Goal:** Persistent storage layer working end-to-end, single-threaded.

**Exit criteria:**
- `prism-storage`: disk manager reads/writes pages; database header validated at open.
- `prism-buffer`: clock-sweep buffer pool with pin/unpin, dirty tracking, WAL invariant enforcement.
- `prism-wal`: append-only WAL with group commit; segment rotation; replay iterator.
- Crash test: kill during write workload, restart, no page corruption.
- Unit and property test coverage for each crate.

**Dependencies:** M0.

**Risks:**
- `O_DIRECT` semantics differ across filesystems. Mitigation: fall back to buffered I/O with explicit fsync where O_DIRECT is rejected.
- Group commit tuning. Mitigation: starting point 1 ms window; tune from benchmark data.

## M2 - Transactions and recovery (target 2026-07-31)

**Goal:** ACID transactions over the storage layer; ARIES recovery proven correct.

**Exit criteria:**
- `prism-core::txn_manager`: begin/commit/abort with TxnId allocation and commit log.
- `prism-core::mvcc`: visibility function, version chain handling, write-write conflict detection.
- `prism-core::lock_manager`: per-RID locks with wait-for graph and deadlock detection.
- `prism-core::recovery`: ARIES analysis/redo/undo with CLRs.
- Fuzzy checkpointing.
- Fault-injection harness running the bank-transfer workload with 5-minute crash cycles; no anomalies in 100 consecutive runs.

**Dependencies:** M1.

**Risks (highest of the project):**
- Recovery edge cases. Mitigation: read ARIES paper carefully, code reviews of every recovery code path, extreme test coverage.
- Lock manager deadlock detection bugs. Mitigation: synthetic deadlock test cases; randomized fairness tests.

## M3 - Three models (target 2026-09-15)

**Goal:** SQL, document, and KV engines all working over the unified record store with cross-model transactions.

**Exit criteria:**
- `prism-index`: B+tree (Lehman-Yao) and extendible hash, both WAL-logged and recoverable.
- `prism-sql`: parser, binder, rewriter, planner, executor for a defined subset of SQL.
- `prism-doc`: insert/find/update/delete with MongoDB-subset query language; field-path indexes.
- `prism-kv`: get/put/delete/range; hash and btree namespaces.
- `prism-core::catalog`: system tables bootstrapped; DDL transactional.
- Cross-model transaction works: SQL + doc + KV in one transaction; commits or rolls back atomically; crash during such a transaction is recovered consistently.
- Multi-model bank-transfer workload passes the harness.

**Dependencies:** M2.

**Risks:**
- Scope is largest at this milestone. Mitigation: prioritize correctness over feature breadth; defer rare SQL constructs.
- SQL parser/binder complexity. Mitigation: use `sqlparser-rs` for parsing; build a narrow binder that rejects unsupported features clearly.
- B+tree concurrency bugs. Mitigation: Lehman-Yao tested extensively with property tests.

## M4 - Surface and harness (target 2026-10-15)

**Goal:** Network protocol, SDK, shell, and the production-grade test harness.

**Exit criteria:**
- `prism-protocol`: encode/decode of the wire protocol.
- `prism-server`: TCP listener, connection lifecycle, TLS, auth (password and mTLS).
- `prism-shell`: interactive client with all meta-commands.
- `prism-sdk-node`: published to npm as `@prism/client`.
- Connection draining on shutdown.
- Idempotency keys.
- 24-hour soak run passes all consistency checks.
- Documentation review: every doc still matches the implementation.

**Dependencies:** M3.

**Risks:**
- napi-rs build process across platforms. Mitigation: prebuilt binaries for major targets; clear source-build fallback.
- Protocol versioning mistakes. Mitigation: code review of every protocol change against `specs/wire-protocol.md`.

## Post-v1 (out of scope)

- Serializable isolation (SSI).
- Replication (Raft).
- Backup and PITR utilities.
- Vacuum / dead-tuple cleanup.
- TOAST / overflow pages for large records.
- Vectorized execution (analytical workloads).
- Bulk loader.
- Online schema migrations.
- Additional SDK languages.

These are not promises for v1.1; they are the obvious next steps once v1.0 ships.

## Cadence

- **Daily:** Per-developer working stream; PRs flow through CI.
- **Weekly:** Status check on the active milestone; risk register update.
- **Per-milestone:** Exit criteria reviewed; demo run; decision to proceed or extend.

## Slip handling

If a milestone slips:
1. Identify what specifically is incomplete.
2. Triage: is it required for the milestone, or can it move out?
3. If required: extend the milestone date and adjust subsequent ones.
4. If not: mark it deferred (with a note to the next milestone or post-v1), proceed.

Slippage of 1-2 weeks per milestone is expected; we plan for it.

## Honest assessment of probabilities

(From `overview/roadmap.md`, reproduced here for the project record:)

- P(M1 done by 2026-06-30): ~80%.
- P(M2 done by 2026-07-31): ~50%.
- P(M3 done by 2026-09-15): ~30%.
- P(M4 done by 2026-10-15, all features): ~15%.

The drop reflects compounding risk: each milestone depends on the previous, and the integration testing dominates the late milestones. We treat the dates as targets, not commitments.

## References

- `overview/roadmap.md` - the narrative version.
- `project/risk-register.md` - risks behind these milestones.
- `operations/build-and-dev.md` - how the work happens.
