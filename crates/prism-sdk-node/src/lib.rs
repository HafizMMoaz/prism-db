//! `prism-sdk-node` — the Node.js SDK.
//!
//! napi-rs bindings exposing connection, transaction, and per-model APIs, with
//! TypeScript definitions auto-generated. Default transport is a remote client
//! over TCP (`prism-protocol`). See `docs/adr/0009-napi-rs-sdk.md` and
//! `docs/specs/sdk-api.md`.
//!
//! Status: skeleton (Phase 4 / M4 not yet started). napi-rs is wired in then;
//! the crate type becomes `cdylib`.
