//! `prism-index` — access methods that are not heap scan.
//!
//! Concurrent B+tree (Lehman-Yao variant), extendible hash index, and the
//! common index traits. Each maps a key space to `RecordId` and uses the
//! record store for fetches. See `docs/components/btree.md` and
//! `docs/components/hash-index.md`.
//!
//! Status: skeleton (Phase 3 / M3 not yet started).
