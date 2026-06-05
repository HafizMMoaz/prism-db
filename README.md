# Prism

A multi-model database engine with unified ACID transactions across relational, document, and key-value access methods.

**Status:** Design phase — v0.1 design documents locked, implementation not started.
**License:** TBD (working assumption: Apache 2.0).
**Primary language:** Rust (edition 2024, MSRV 1.85).
**Target platforms:** Linux (x86_64, aarch64), macOS (aarch64), Windows (x86_64). All three are first-class, CI-tested, and shippable.

---

## What this is

Prism is a single storage engine that exposes three first-class data models — relational tables with SQL, JSON-like documents, and ordered key-value pairs — all sharing one buffer pool, one write-ahead log, and one transaction manager. A single transaction can mutate rows, documents, and KV pairs atomically.

This is not a wrapper around three databases. It is one engine with three access methods on top of a unified record store.

## What this is not

- Not a distributed database. Single-node only. Replication is explicitly out of scope for v1.
- Not a wire-compatible replacement for any existing system. We do not speak Postgres protocol, MongoDB protocol, or Redis RESP.
- Not a research project. Every design choice has prior art in production systems; this is engineering, not novel CS.
- Not an analytical engine. OLTP workloads only. Columnar storage and vectorized execution are out of scope for v1.

## Why it exists

Polyglot persistence — running Postgres, MongoDB, and Redis side by side — is the default answer for applications that need multiple data shapes. It works, but it pushes consistency to the application layer. Cross-store transactions become saga patterns or two-phase commits between systems that were never designed to cooperate. The operational footprint is three services, three backup strategies, three failure modes.

Prism asks a narrower question: if the storage primitive underneath these three shapes is the same — a tuple in a slotted page, written through a WAL, versioned by a transaction ID — can the three access methods share that primitive and inherit ACID across model boundaries for free?

The answer, going in, is yes. The design documents in this repository argue why and lay out how.

## Documentation map

The design corpus lives entirely in [`docs/`](docs/). Read in this order if you are new:

1. [`docs/overview/executive-summary.md`](docs/overview/executive-summary.md) — one-page summary
2. [`docs/overview/vision-and-scope.md`](docs/overview/vision-and-scope.md) — goals, non-goals, success criteria
3. [`docs/architecture/system-architecture.md`](docs/architecture/system-architecture.md) — components and how they fit
4. [`docs/adr/`](docs/adr/) — every significant design decision and its rationale
5. [`docs/components/`](docs/components/) — per-component design docs
6. [`docs/specs/`](docs/specs/) — wire-level specifications

## Project state

| Phase | Status |
|---|---|
| Design docs (this repo) | In progress |
| Core foundation (disk, buffer pool, WAL) | Not started |
| Transactions + recovery | Not started |
| Access methods (SQL, document, KV) | Not started |
| Network protocol + SDK | Not started |
| Hardening + Jepsen-style testing | Not started |

See [`docs/project/milestones.md`](docs/project/milestones.md) for the dated plan and [`docs/project/risk-register.md`](docs/project/risk-register.md) for what is most likely to go wrong.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md). Short version: design docs are reviewed via pull request and discussed in writing. Code is not accepted until the relevant design doc and ADR are merged.
