# Architecture Decision Records

ADRs document significant decisions: their context, the choice made, alternatives considered, and consequences. They are immutable once accepted; superseding a decision means writing a new ADR that references the old one with status `Superseded by ADR XXXX`.

## Format

```
# ADR XXXX: Title

**Status:** Proposed | Accepted | Superseded by ADR YYYY | Deprecated
**Date:** YYYY-MM-DD
**Deciders:** Names
**Context:**
[The forces at play. What problem are we solving? What constraints exist?]

**Decision:**
[The chosen approach.]

**Alternatives considered:**
[What else was on the table and why it was rejected.]

**Consequences:**
[What this enables, what this rules out, what becomes harder.]

**References:**
[Papers, prior art, related ADRs.]
```

## Index

| # | Title | Status |
|---|---|---|
| 0001 | [Use Rust as the implementation language](0001-language-rust.md) | Accepted |
| 0002 | [Page-based storage with slotted pages](0002-page-based-storage.md) | Accepted |
| 0003 | [Physiological WAL with ARIES recovery](0003-physiological-wal-aries.md) | Accepted |
| 0004 | [MVCC with snapshot isolation](0004-mvcc-snapshot-isolation.md) | Accepted |
| 0005 | [Unified record format across models](0005-unified-record-format.md) | Accepted |
| 0006 | [Single WAL and transaction manager for cross-model transactions](0006-single-wal-cross-model.md) | Accepted |
| 0007 | [Clock-sweep buffer pool replacement](0007-clock-sweep-buffer-pool.md) | Accepted |
| 0008 | [Binary length-prefixed TCP wire protocol](0008-binary-wire-protocol.md) | Accepted |
| 0009 | [Node.js SDK via napi-rs](0009-napi-rs-sdk.md) | Accepted |
| 0010 | [Volcano iterator execution model](0010-volcano-executor.md) | Accepted |

## Convention

When a design document or code comment references "ADR 0003," that is a direct citation to the file `0003-physiological-wal-aries.md`. ADR numbers are never reused.
