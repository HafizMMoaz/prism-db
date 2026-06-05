//! `prism-sql` — the relational engine.
//!
//! SQL parser (built on `sqlparser-rs`), binder (resolves identifiers against
//! the catalog), logical and physical planner, and a Volcano executor over the
//! supported SQL subset. Predicate pushdown and basic index selection only;
//! no cost-based optimizer in v1. See `docs/components/sql-engine.md`.
//!
//! Status: skeleton (Phase 3 / M3 not yet started).
