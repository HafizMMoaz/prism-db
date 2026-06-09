//! `prism-server` — the server and the embedded in-process API.
//!
//! This crate is the boundary between the wire protocol and the cross-model
//! engine. [`Database`] assembles the shared storage stack (disk, WAL, buffer
//! pool, transaction manager, record store) and the three engines on top of it;
//! [`Session`] is the per-connection state machine that decodes a
//! [`prism_protocol::Message`], runs it against the right engine in the session's
//! transaction (explicit or implicit), and produces the response. Because all
//! three engines share one store and one transaction manager, a single session
//! transaction spans SQL, document, and KV atomically. See
//! `docs/components/network-server.md`.
//!
//! **Status (Phase 4 / M4, in progress):** the synchronous, in-process
//! dispatcher — the embedded API, and the core the network layer will wrap. The
//! Tokio TCP listener, TLS, authentication, idempotency, and cancellation are a
//! follow-up increment; see [`Session`] for the per-request simplifications in
//! this slice.

pub mod database;
pub mod error;
pub mod session;

pub use database::{Config, Database};
pub use error::{Result, ServerError};
pub use session::Session;
