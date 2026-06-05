//! `prism-server` — the server, and the embedded in-process API.
//!
//! TCP listener with TLS, connection state machine, authentication, the query
//! dispatcher (routes to the SQL/document/KV engine by request type), and
//! implicit/explicit transaction handling. The library form exposes an
//! in-process API for embedded use and for `prism-bench`. See
//! `docs/components/network-server.md`.
//!
//! Status: skeleton (Phase 4 / M4 not yet started).
