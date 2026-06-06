//! `prism-core` — the transactional record store. The heart of the engine.
//!
//! This crate owns transaction lifecycle, MVCC visibility, the lock manager,
//! ARIES recovery, and the catalog — one of each, shared across all three
//! access methods. See `docs/components/transaction-manager.md`,
//! `mvcc.md`, `recovery.md`, and `lock-manager.md`.
//!
//! Implemented so far (Phase 2 / M2, in progress):
//! - [`record`] — the 24-byte MVCC tuple header and [`RecordId`].
//! - [`txn`] — the [`TxnManager`], [`TxnHandle`], snapshots, and commit log.
//! - [`visibility`] — the snapshot-isolation visibility function.
//!
//! Still to come this milestone: the record store (MVCC tuple ops over the
//! buffer pool), the lock manager, and ARIES recovery.

pub mod error;
pub mod record;
pub mod txn;
pub mod visibility;

pub use error::{CoreError, Result};
pub use record::{
    FLAG_FORWARDED, FLAG_HAS_PREV, FLAG_LOCKED, FLAG_TOMBSTONE, RECORD_HEADER_SIZE, RecordHeader,
    RecordId,
};
pub use txn::{CommitLog, CommitStatus, Snapshot, TxnHandle, TxnManager, TxnMode};
pub use visibility::visible;

/// A transaction identifier. Monotonic, never reused.
pub type TxnId = u64;

/// Sentinel meaning "no transaction" — used for `xmax = 0` (not deleted).
pub const NO_TXN: TxnId = 0;
/// The bootstrap transaction that creates the catalog at database creation.
/// Its effects are always visible.
pub const BOOTSTRAP_TXN: TxnId = 1;
/// The first transaction id handed out to user transactions.
pub const FIRST_USER_TXN: TxnId = 2;
