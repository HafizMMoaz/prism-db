//! `prism-index` — access methods that are not heap scan.
//!
//! The ordered [`BTree`] (point + range) and, later, the extendible hash index.
//! Both map a key space to `RecordId` and live in pages fetched through the
//! buffer pool. See `docs/components/btree.md` and `docs/components/hash-index.md`.
//!
//! **Scope so far:** a correct single-threaded, unique-key B+tree (upsert) with
//! leaf/internal/root splits and ordered range scan, validated against a
//! `BTreeMap` oracle. Deferred to later increments (large and orthogonal): the
//! Lehman-Yao concurrent latch protocol + high-key right-chase, WAL-logging for
//! crash recovery, duplicate-key (non-unique) index support, and the extendible
//! hash index.

pub mod btree;
pub mod error;

pub use btree::BTree;
pub use error::{IndexError, Result};
