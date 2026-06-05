//! `prism-protocol` — wire protocol types.
//!
//! Request/Response enums and their stable binary serialization, plus protocol
//! versioning. Pure data definitions with no I/O, so both the server and every
//! client can depend on it without pulling in the engine. See
//! `docs/specs/wire-protocol.md` and `docs/adr/0008-binary-wire-protocol.md`.
//!
//! Status: skeleton (Phase 4 / M4 not yet started).
