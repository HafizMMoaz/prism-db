# PrismDB client SDKs

Official client libraries for [PrismDB](https://github.com/HafizMMoaz/prism-db). Every SDK is a **pure
implementation of the binary wire protocol** (`docs/specs/wire-protocol.md`)
over a TCP (or TLS) socket - no native add-ons, no FFI, nothing to build on the
user's machine beyond the language's own toolchain. The byte layouts are kept in
lockstep with the Rust `prism-protocol` crate; the encoders are validated
byte-for-byte against one another.

| Language | Directory | Package | Install |
|----------|-----------|---------|---------|
| Node / TypeScript | [`node/`](node) | `@prismdb/client` | `npm install @prismdb/client` |
| Python | [`python/`](python) | `prismdb` | `pip install prismdb` |
| Java | [`java/`](java) | `dev.prism:prism-client` | Maven dependency |
| C# / .NET | [`dotnet/`](dotnet) | `PrismDb.Client` | `dotnet add package PrismDb.Client` |
| C++ (17) | [`cpp/`](cpp) | header + `prism.cpp` | vendor or `make` |
| C (99/11) | [`c/`](c) | `prism.h` + `prism.c` | vendor or `make` |
| PHP | [`php/`](php) | `prismdb/client` | `composer require prismdb/client` |

## Common surface

Each SDK exposes the same capabilities, named idiomatically per language:

- **Connect & authenticate** - `Hello`/`Auth` handshake, optional connect-time
  database selection, optional TLS.
- **SQL** - execute statements with positional `$1, $2, …` parameters; rows come
  back keyed by column name and in column order, plus an affected-row count.
- **Key/value** - `get` / `put` / `delete` over named namespaces.
- **Documents** - `insertOne` / `insertMany`, `find` / `findOne`, `count`,
  `update*`, `delete*`, with `Q` filter builders (`eq`, `gt`, `in`, `and`, …) and
  `U` update builders (`set`, `inc`, `unset`).
- **Transactions** - `begin` / `commit` / `abort`; one client = one session, so
  calls between `begin` and `commit` run in that transaction (atomic across the
  SQL, document, and KV models).
- **Errors** - a typed server error carrying the wire `code`, `sqlstate`,
  `detail`, and source `position`, plus a protocol/decode error type.

## Value mapping

All SDKs map their language's native scalars onto the wire type tags from
`docs/specs/record-format.md`. Integers default to **Int64**; each SDK provides
explicit helpers (`int32`, `float64`, `timestamp`, …) to pin a different wire
type, and an `ObjectId` type for document ids.

## Status

These are v0.1 clients. Known shared limitations: streamed (multi-frame)
SQL/document results are not yet reassembled, KV `range`/`scan` are follow-ups,
and the C and C++ cores do not yet implement TLS (use a TLS-terminating proxy or
another SDK). See each SDK's README for specifics and run instructions.
