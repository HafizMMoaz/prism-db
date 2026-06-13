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
//! **Status (Phase 4 / M4, in progress):** the in-process dispatcher
//! ([`Session`], the embedded API) plus the Tokio TCP front-end ([`Server`])
//! that wraps it — a client can now talk to PrismDB over a socket. TLS,
//! authentication, idempotency, cancellation, and connection/transaction limits
//! are a follow-up; see [`Session`] for the per-request simplifications and
//! [`server`] for the deferred network features.

pub mod auth;
pub mod catalog;
pub mod database;
pub mod dump;
pub mod error;
pub mod server;
pub mod session;
pub mod tls;

pub use database::{Config, Database};
pub use dump::{ImportStats, export_to_string, import};
pub use error::{Result, ServerError};
pub use server::{Server, ServerConfig};
pub use session::Session;
