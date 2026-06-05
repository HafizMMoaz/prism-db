//! `prism-kv` — the key-value engine.
//!
//! Namespace abstraction with get/put/delete and range scans. Hash index for
//! point lookups (default); opt-in ordered B+tree index for ranges. The
//! simplest of the three access methods — ships first as an end-to-end smoke
//! test of the unified store. See `docs/components/kv-engine.md`.
//!
//! Status: skeleton (Phase 3 / M3 not yet started).
