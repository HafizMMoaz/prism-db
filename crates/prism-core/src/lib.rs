//! `prism-core` — the transactional record store. The heart of the engine.
//!
//! Transaction manager (TxnId allocation, commit log, active-txn table), MVCC
//! tuple operations and visibility, version-chain traversal, the lock manager
//! (per-RID locks, wait-for graph, deadlock detection), the ARIES recovery
//! driver, and catalog access. One of each, shared across all three access
//! methods — there is no such thing as a "cross-model" transaction here.
//! See `docs/components/mvcc.md`, `transaction-manager.md`, `recovery.md`.
//!
//! Status: skeleton (Phase 2 / M2 not yet started).
