# Component: Network Server

**Crate:** `prism-server`
**Status:** Accepted
**Last updated:** 2026-05-15

## Purpose

The network server accepts TCP connections from clients, authenticates them, frames the wire protocol, dispatches requests to the appropriate engine, and ships responses back. It is the boundary between the trusted internals and the untrusted network.

## Topology

```
                     ┌──────────────────────────────────┐
                     │       Tokio runtime              │
                     │                                  │
TCP listener ─────►  │  Accept loop                     │
(port 4444)          │                                  │
                     │  ├── Connection task 1           │
                     │  │   ├── frame reader            │
                     │  │   ├── frame writer            │
                     │  │   └── request handler         │
                     │  ├── Connection task 2           │
                     │  └── ...                         │
                     │                                  │
                     └──────────────────────────────────┘
                                  │
                                  ▼
                     ┌──────────────────────────────────┐
                     │   Query dispatcher (sync)        │
                     │     routes to:                   │
                     │       SQL engine                 │
                     │       Document engine            │
                     │       KV engine                  │
                     └──────────────────────────────────┘
```

## Runtime

One Tokio runtime, configured with N worker threads (default = available cores). Async I/O for the connection lifecycle; synchronous CPU-bound work (parsing, execution) happens on `spawn_blocking` to avoid stalling the I/O reactor.

The buffer pool and WAL appender are synchronous; calls into them happen inside `spawn_blocking` blocks. This is the standard Tokio pattern for mixing async I/O with synchronous compute.

## Connection lifecycle

```
1. Accept loop: TcpListener::accept() returns a TcpStream.
2. Spawn a connection task. The accept loop is back to accepting.
3. Connection task:
   a. TLS handshake if configured.
   b. Receive Hello frame. Validate protocol version. Send HelloAck.
   c. Receive Auth frame. Validate credentials. Send AuthOk or AuthFail (and close on fail).
   d. Enter the request loop:
      - Read a frame.
      - Decode to Request.
      - Dispatch (begin/commit/abort handled at this layer; queries dispatched to engines).
      - Encode response, write frame.
   e. On connection drop, abort any active transaction.
```

## Frame protocol

See `specs/wire-protocol.md` for the byte layout. Briefly:

```
┌──────────────┬─────────────────────────────┐
│ length (u32) │ payload                     │
└──────────────┴─────────────────────────────┘
```

Length includes everything after itself. Max frame size is configurable (default 64 MiB) to prevent memory exhaustion attacks.

Framing is a tiny state machine. Backpressure: if the writer is slow (client not reading), the per-connection write buffer fills, and the request loop pauses until the writer drains.

## Authentication

Two mechanisms supported:

1. **Password (default).** Client sends `Auth { username, password }`. Server fetches the user's stored hash from the catalog, compares with `scrypt::verify`. On success, the session is associated with the user's OID.

2. **mTLS (optional).** Client presents a certificate. Server extracts the certificate's subject CN, looks up the user by name. No separate password step.

Failed authentication closes the connection after a small delay (50-200 ms randomized) to slow brute force.

## TLS

If enabled, the listener performs TLS handshake before any frame is read. Configuration:

```toml
[network.tls]
enabled = true
cert_path = "/etc/prism/server.crt"
key_path = "/etc/prism/server.key"
client_ca_path = "/etc/prism/clients.crt"   # for mTLS
min_version = "1.3"
```

We use `rustls` for TLS. No OpenSSL dependency.

## Transaction tracking

Each connection has at most one active transaction. State:

```rust
enum SessionTxn {
    None,
    Implicit(TxnHandle),   // single-statement
    Explicit(TxnHandle),   // user issued BEGIN
}
```

- `Begin` request: must be in `None` state; transitions to `Explicit`.
- `Commit`/`Abort`: must be in `Explicit`; transitions to `None`.
- `Query`/`KvOp`/`DocOp` while in `None`: server begins an `Implicit` transaction, runs, commits, transitions back to `None`. All in one logical step from the client's perspective.
- `Query`/`KvOp`/`DocOp` while in `Explicit`: runs in that transaction.
- Connection drop in `Explicit`: server aborts the transaction.

## Request dispatch

```
match request {
    Request::Sql(sql, params) => sql_engine.execute(txn, sql, params)
    Request::DocOp(op) => doc_engine.execute(txn, op)
    Request::KvOp(op) => kv_engine.execute(txn, op)
    Request::Begin => txn_manager.begin()
    Request::Commit => txn_manager.commit(...)
    Request::Abort => txn_manager.abort(...)
}
```

Dispatch is synchronous; the executor produces a `Response`, which is then encoded and written.

## Cancellation

Each request carries a `request_id`. Clients can send `Cancel { request_id }` on a separate connection (or the same, if the request loop is multiplexed) to abort an in-flight query.

Cancellation is cooperative: the executor checks a cancellation token at each operator's `next()` call. When set, the operator returns `Err(Cancelled)`, the request is aborted, and the client receives a `Cancelled` response.

## Backpressure and limits

- **Max connections per user**: default 100. Enforced at accept; new connections beyond the limit get `TooManyConnections` immediately.
- **Max concurrent transactions per user**: default 50.
- **Max query memory**: enforced by the executor (see `components/sql-engine.md`).
- **Query timeout**: default 30 seconds. Configurable per session.
- **Frame size cap**: 64 MiB. Frames larger are rejected.

## Idempotency

Every modifying request can carry an idempotency key (`u128`, opaque to the server). The server records `(idempotency_key, txn_id, response)` for committed transactions. If a request arrives with a key that matches a recent committed transaction, the server returns the recorded response without re-executing.

Idempotency records expire after a configurable window (default 24 hours).

This solves the at-least-once retry problem: if a commit succeeds but the response is lost, the client retries with the same key and gets the original response.

## Connection draining

On graceful shutdown:
1. Listener stops accepting new connections.
2. Existing connections are notified (a `Notice` frame).
3. Connections are given a grace window (default 30 seconds) to complete in-flight requests.
4. After grace, remaining connections are closed; their transactions abort.

## Configuration

```toml
[network]
bind = "0.0.0.0:4444"
max_connections = 10000
max_frame_size_mib = 64
default_query_timeout_secs = 30
idempotency_window_secs = 86400

[network.tls]
enabled = false
# ...
```

## Metrics

- `prism_net_connections_active` (gauge)
- `prism_net_connections_total` (counter)
- `prism_net_authentication_failures_total`
- `prism_net_requests_total{type}`
- `prism_net_request_duration_seconds{type}` (histogram)
- `prism_net_bytes_received_total`
- `prism_net_bytes_sent_total`

## Testing

- Unit: framing, request decoding, response encoding.
- Integration: end-to-end against the in-memory test runtime.
- Fuzzing: framing and protocol decoding with `cargo fuzz`.
- Concurrent stress: 1000 connections, sustained throughput.

## References

- ADR 0008 - protocol choice.
- `specs/wire-protocol.md` - normative byte layout.
- `components/transaction-manager.md` - session txn handling.
- Tokio documentation for the async model.
