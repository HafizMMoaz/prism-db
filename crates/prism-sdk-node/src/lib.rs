//! `prism-sdk-node` - reserved crate for a future native (napi-rs) Node binding.
//!
//! The shipping Node.js SDK is **pure TypeScript** over the wire protocol, at
//! [`sdks/node`](../../sdks/node) (`@prismdb/client`): it speaks
//! `prism-protocol` directly over a TCP/TLS socket, so it needs no native build
//! and runs anywhere Node does. That diverges from `docs/adr/0009-napi-rs-sdk.md`
//! (which proposed napi-rs to reuse the Rust client); the napi path also
//! conflicts with the current zero-Node CI (`cargo test --all-features`), so it
//! is deferred.
//!
//! This crate remains a plain `lib` skeleton; if an in-process native binding is
//! wanted later, it would be wired up here behind a dedicated CI job.
