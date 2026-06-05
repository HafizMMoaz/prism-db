# ADR 0009: Node.js SDK via napi-rs

**Status:** Accepted
**Date:** 2026-05-15

## Context

The primary client target for v1 is Node.js — JavaScript and TypeScript developers building applications against Prism. The SDK is what most users will see; its ergonomics directly determine adoption.

Ways to provide a Node.js SDK:

1. **Pure JavaScript implementation of the wire protocol.** No native code; clients ship as plain `npm install`. Slower (serialization in JS) but maximum portability.

2. **Native addon via `node-addon-api` (C++).** Wraps a C++ client. Faster than pure JS. Build complexity (`node-gyp`).

3. **Native addon via `napi-rs` (Rust).** Same N-API surface as `node-addon-api`, but the addon is written in Rust. Generated TypeScript definitions. Prebuilt binaries via GitHub Actions, with `node-gyp` fallback for unsupported platforms.

4. **WebAssembly module.** Portable, no native build. Slower than native. Awkward for blocking I/O.

5. **Thin remote client + extensive server-side logic.** Most logic on the server; the SDK is a transport wrapper.

## Decision

Prism v1 ships a Node.js SDK as a **napi-rs native addon** that speaks the Prism binary wire protocol over TCP. The SDK is a thin remote client; the server does the work.

Prebuilt binaries for:
- Linux x86_64 (glibc and musl)
- Linux aarch64 (glibc)
- macOS aarch64
- Windows x86_64 (best-effort; tested but not blocked-on)

## Alternatives considered

### Pure JavaScript
**For:** Universal install. No native build. Smaller attack surface (no native code).

**Against:** All wire-protocol serialization happens in JavaScript. For a workload with thousands of small operations per second per client, this is a measurable overhead. More importantly: we want to share the protocol implementation between the SDK and an embedded mode (where Prism is linked directly into the Node process). The embedded mode requires native code. Maintaining two implementations of the protocol — one in JS, one in Rust — is a long-term tax.

We will likely provide a pure-JS fallback as a separate package for edge environments (Cloudflare Workers, etc.) that can't run native addons. But the primary SDK is native.

### `node-addon-api` (C++)
**For:** Mature. Production-tested by many large native modules. The default choice five years ago.

**Against:** Requires writing C++ to bridge Rust core (or rewriting parts of the SDK in C++). The mixing of Rust core + C++ glue is brittle. `node-gyp` is famously painful. Type definitions are hand-written.

### WebAssembly
**For:** Portable. No native build. Easy distribution.

**Against:** Blocking sockets are not a thing in WebAssembly; we would need WASI or browser fetch, both of which complicate the implementation. Performance for the binary protocol's deserialization is acceptable but slower than native. For Node.js specifically, native is the right tool; WASM is right for browsers.

### Thin remote vs. embedded mode
**For thin remote (chosen):** Clean architecture. Server does the work. Multiple SDK versions can connect to one server. Operational simplicity.

**For embedded:** Lower latency (no TCP round-trip). Fits use cases where the application and database are the same process (CLI tools, single-machine scripts).

We do both, with the embedded mode as a v1.1 deliverable. v1 ships the remote SDK because the wire protocol must exist anyway for the shell and for non-Node clients.

## Why napi-rs

1. **Rust everywhere.** Both the engine and the SDK addon are Rust. One language. One toolchain. No FFI cliff.

2. **Auto-generated TypeScript definitions.** `napi-rs` reads Rust function signatures and produces `.d.ts` files. Manual type-stub maintenance is a long-tail bug source eliminated.

3. **Prebuilt binary distribution.** `napi-rs` integrates with GitHub Actions to publish per-platform binaries to npm. The user runs `npm install @prism-db/sdk` and gets the right binary; no compilation on install.

4. **Active project.** `napi-rs` is used by sharp, parcel, swc, prisma — mainstream Node.js infrastructure. Not a research toy.

5. **Easy bridging to in-process embedded mode later.** Same Rust types used by the server can be exposed directly to JavaScript without a network round-trip.

## SDK shape

```typescript
import { Client } from "@prism-db/sdk";

const client = await Client.connect({
  host: "localhost",
  port: 4444,
  username: "alice",
  password: "...",
  tls: { ca: "..." }, // optional
});

// SQL
const result = await client.sql("SELECT * FROM users WHERE id = $1", [1]);

// Document
const doc = await client.documents("events").insertOne({ user_id: 1, type: "login" });
const found = await client.documents("events").findOne({ user_id: 1 });

// KV
await client.kv.put("session:abc", Buffer.from("..."));
const value = await client.kv.get("session:abc");

// Cross-model transaction
await client.transaction(async (txn) => {
  await txn.sql("INSERT INTO orders ...", []);
  await txn.documents("events").insertOne({ ... });
  await txn.kv.put("cache:order:1", ...);
});  // commits if the callback returns, rolls back if it throws
```

Detail in `specs/sdk-api.md`.

### Error model
The SDK throws typed errors. Every server-side error has a stable error code (e.g., `PRISM_SERIALIZATION_FAILURE`, `PRISM_DEADLOCK`, `PRISM_CONNECTION_LOST`). Error codes are documented and stable across versions.

### Connection management
The SDK manages a single connection by default. A pool helper is available for applications with concurrent transactions. Connection failure is propagated as an error; the SDK does not silently reconnect mid-transaction (the transaction is aborted; the caller decides whether to retry).

### Idempotency
Every transaction carries an idempotency key (auto-generated or caller-supplied). If a commit's response is lost on the wire and the client retries with the same key, the server returns the existing result instead of re-executing.

## Distribution

- Package name: `@prism-db/sdk`
- Versioning: independent of the server, but a compatibility matrix is published.
- Release: GitHub Actions builds per platform, publishes to npm, includes prebuilt binary in tarball.
- Source: same monorepo as the engine, under `crates/prism-sdk-node`.

## Consequences

### Enabled
- One-command install, no build on the user's machine.
- Generated TypeScript types stay in sync.
- Performance of native code without writing C++.
- Path to embedded mode using the same crate.

### Constrained
- Platforms not in our prebuilt matrix require `cargo` to be installed on the user's machine (with fallback build). Acceptable.
- The SDK depends on a Node.js version Rust's napi-rs supports (Node 16+). Documented.
- Cross-compilation for Windows is a moving target; we treat Windows as best-effort, not blocking.

### Required follow-on
- SDK API surface details → `specs/sdk-api.md`.
- Release process → `project/release-process.md` (TBD).
- Client-side connection pooling → SDK implementation.

## References

- napi-rs: <https://napi.rs/>
- ADR 0001 — Rust choice; napi-rs is contingent on it.
- ADR 0008 — wire protocol; the SDK speaks it.
- `specs/sdk-api.md` — surface details.
