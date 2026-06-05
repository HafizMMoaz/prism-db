# ADR 0008: Binary length-prefixed TCP wire protocol

**Status:** Accepted
**Date:** 2026-05-15

## Context

Clients talk to the Prism server over some protocol. Choices:

1. **HTTP / REST.** Easy to implement, debuggable with curl, every language has clients. Large overhead per request (HTTP headers, JSON encoding), poor fit for transactional workloads with many small requests.

2. **gRPC.** Modern, well-tooled, schema-evolved via protobuf. HTTP/2 multiplexing handles many concurrent requests well. But adds protobuf as a dependency and a generated-code surface.

3. **PostgreSQL wire protocol.** Compatibility with existing tooling. But: large, complex, includes features we don't support (cursors, large objects, copy-in/out, etc.), commits us to relational-shaped requests when our model is broader.

4. **MongoDB wire protocol.** Same problem in the document direction.

5. **Custom binary protocol over TCP.** Smallest overhead, exactly the surface we need, no compatibility baggage. Clients must use our SDK; no curl debugging.

## Decision

Prism uses a **custom binary length-prefixed protocol over TCP**, with optional TLS.

The protocol is documented in `specs/wire-protocol.md`. Summary:

- Length-prefixed frames: 4 bytes big-endian length, then payload.
- Payload is a versioned binary message encoded via `bincode` (or a similar stable serializer; final choice in the spec).
- Messages: `Hello`, `Auth`, `Begin`, `Commit`, `Abort`, `Query`, `KvOp`, `DocOp`, `Subscribe`, `Heartbeat`, plus responses.
- Multiplexing within a connection: requests carry a `request_id`; responses match.

## Alternatives considered

### HTTP / REST
**For:** Universal, debuggable, no SDK required.

**Against:** Per-request overhead is significant when transactions involve many small operations. A connection-oriented protocol with multiplexing is far more appropriate for an OLTP workload. Also: REST and transactions are an awkward fit — keeping a transaction context across requests requires either session affinity or token round-tripping, both inferior to a stateful connection.

We will likely provide an HTTP gateway as a separate product for ad-hoc tooling, but it is not the primary protocol.

### gRPC
**For:** Battle-tested, multiplexed, generated clients in every language.

**Against:** Protobuf is large and adds dependencies (`prost`, `tonic`). HTTP/2 framing adds bytes per message. The schema-evolution story is excellent but we don't need it for v1 — we control both ends. gRPC is a defensible choice; we are deferring it on simplicity grounds.

### Wire-compatibility (Postgres or Mongo protocol)
**For:** Existing tools (psql, mongo shell, ORMs, drivers) work immediately.

**Against:** Each protocol is large (Postgres frontend/backend protocol is ~30 message types) and includes features we don't support, so we would be implementing a partial protocol that silently fails on unsupported messages. Worse: it commits us to data shapes that match the originating system. Postgres-compatible means looking like a SQL database, which buries the document and KV models.

Implementing wire compatibility is also a substantial project on its own — Cockroach took years to converge on Postgres wire compatibility. We are not signing up for that scope.

### Multiple protocols (REST + binary)
**For:** Best of both.

**Against:** Two protocols mean two code paths, two test surfaces, two security audits. The binary protocol is required; REST is a nice-to-have that can be a thin gateway over the binary protocol if we want it later.

## What the protocol does

Each TCP connection is a session. A session:
- Authenticates once with `Hello` + `Auth`.
- Issues requests; responses correlate via `request_id`.
- Carries transactions implicitly within the connection or explicitly via `Begin` / `Commit` / `Abort`.
- Receives unsolicited messages (e.g., heartbeat pings, async notifications) on the same channel.

Framing:
```
┌──────────────┬─────────────────────────────┐
│ length (u32) │ payload (length bytes)      │
└──────────────┴─────────────────────────────┘
```

Payload structure:
```
┌────────────┬─────────────┬──────────────────────────────┐
│ version u8 │ msg_type u8 │ msg-specific encoded payload │
└────────────┴─────────────┴──────────────────────────────┘
```

Version is checked on the first message of a connection. Mismatched versions terminate the connection with a clear error.

Message-specific payloads use `bincode` with our own enum schemas. We control both the server and the SDK, so we don't need protobuf-grade schema evolution; we will version the protocol as a whole.

## Connection-level transaction tracking

When a client sends `Begin`, the server allocates a TxnId and associates it with the connection. Subsequent `Query`/`KvOp`/`DocOp` requests on the same connection use that TxnId until `Commit` or `Abort`. The session state machine is:

```
              ┌─────────────────────────────┐
              │  Authenticated, no txn      │
              └────────┬──────────────────┬─┘
                       │ Begin            │ Query (single-stmt txn)
                       ▼                  │
              ┌─────────────────────┐     │
              │  In transaction     │◄────┘
              └─┬──────────────────┬┘
                │ Commit / Abort   │ Query, KvOp, DocOp (within txn)
                ▼                  │
              ┌──────────────────────┐
              │  Authenticated, no txn│
              └──────────────────────┘
```

A connection drop while a transaction is active aborts the transaction.

## TLS

TLS is optional and configured at the server. When enabled, the server presents a certificate; clients verify. Mutual TLS is supported for service-to-service deployments.

Connections start in plaintext on the listening port unless TLS-on-port is configured. We do not support STARTTLS-style upgrades in v1 (added complexity, marginal value).

## Consequences

### Enabled
- Compact wire format, minimal per-request overhead.
- Stateful connections suit transactional workloads.
- Multiplexed requests on one connection.
- Full control over the surface: we add what we need, nothing more.

### Constrained
- No psql, no mongo shell, no curl. Clients must use the SDK or our shell.
- Protocol versioning is whole-protocol, not per-field. Breaking changes require a major version bump.
- Limited language coverage initially: v1 ships with Node.js SDK only. Other languages must wait or use HTTP-via-gateway.

### Required follow-on
- Byte-level message format → `specs/wire-protocol.md`.
- Authentication flow → `specs/wire-protocol.md` (Auth section).
- Connection lifecycle and error handling → `components/network-server.md`.

## References

- ADR 0009 — Node.js SDK; the protocol is what the SDK speaks.
- `specs/wire-protocol.md` — normative.
- For comparison, the Postgres frontend/backend protocol documentation (`https://www.postgresql.org/docs/current/protocol.html`) — what we are not implementing.
