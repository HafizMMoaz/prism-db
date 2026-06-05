# Specification: Wire Protocol

**Status:** Accepted (normative)
**Last updated:** 2026-05-15
**Version:** 1.0
**Default port:** 4444

This document specifies the wire protocol clients use to communicate with a Prism server. All multi-byte integers are little-endian unless otherwise noted.

## Transport

TCP. TLS 1.3 is optional but recommended. The default port is 4444.

The connection is binary, full-duplex, and message-framed (not streaming). A client may have multiple in-flight requests on one connection, distinguished by `request_id`.

## Framing

Every message on the wire is a frame:

```
┌──────────────┬─────────────────────────────────────┐
│ length: u32  │ payload: variable                   │
└──────────────┴─────────────────────────────────────┘
```

`length` is the size of `payload` in bytes (does not include the length field itself).

A frame larger than `max_frame_size` (default 64 MiB) causes the server to close the connection with no response.

The framing is symmetric: client → server and server → client both use this frame format.

## Payload header

Every payload begins with a 12-byte common header:

```
Offset  Size  Field
─────   ────  ─────
0       1     message_type
1       3     reserved
4       4     request_id      u32, client-assigned for client→server frames
                              echoed back in the corresponding server→client frame
8       4     reserved
```

Total: 12 bytes. The rest of the payload is message-type-specific.

For server-initiated frames (e.g., `Notice`), `request_id` is 0.

## Connection handshake

After TCP connect (and TLS handshake if enabled):

### 1. Client → Server: `Hello` (message_type = 0x01)

```
protocol_version: u32     (= 1)
client_name:      u16 length + UTF-8 bytes
client_version:   u16 length + UTF-8 bytes
features:         u32 bitmask  (reserved, send 0)
```

### 2. Server → Client: `HelloAck` (0x02)

```
status:           u8     (0 = OK, non-zero = error)
server_version:   u16 length + UTF-8 bytes
features:         u32 bitmask
session_id:       u128 random  (logged for traceability)
```

If status != 0, the server closes the connection after sending. Errors include `ProtocolVersionMismatch`, `Overloaded`.

### 3. Client → Server: `Auth` (0x03)

```
mechanism: u8           (1 = password, 2 = mtls)
[if password:]
  username: u16 length + bytes
  password: u16 length + bytes
[if mtls:]
  username: u16 length + bytes
  (the certificate is already presented at TLS layer)
```

### 4. Server → Client: `AuthAck` (0x04)

```
status:   u8       (0 = OK, 1 = bad_credentials, 2 = no_such_user)
user_oid: u64      (if OK; else 0)
```

If status != 0, server closes the connection. The session is now authenticated.

## Transaction control

### `Begin` (client → server, 0x10)

```
mode: u8       (0 = read_write, 1 = read_only)
```

### `Commit` (client → server, 0x11)

```
idempotency_key: u128    (0 = no key)
```

### `Abort` (client → server, 0x12)

```
(no body)
```

### `TxnAck` (server → client, 0x13)

```
status:        u8       (0 = OK, other codes for errors)
txn_id:        u64      (the assigned TxnId on begin; current on others)
commit_lsn:    u64      (on commit; 0 otherwise)
```

## Query: SQL

### `SqlExecute` (client → server, 0x20)

```
sql:           u32 length + UTF-8 bytes
param_count:   u16
params:        param_count × TaggedValue
options:       u32 bitmask
                bit 0: return_rows (otherwise just affected count)
```

`TaggedValue`:
```
type_tag:  u8    (matches document type tags from specs/record-format.md)
value:     bytes (length depends on type_tag, same encoding as document fields)
```

### `SqlResult` (server → client, 0x21)

Sent in possibly multiple frames for streaming. The first frame includes column metadata:

```
status:           u8
affected_rows:    u64       (for INSERT/UPDATE/DELETE; 0 for SELECT)
column_count:     u16
columns:          column_count × ColumnDesc
row_count:        u32       (rows in this frame)
rows:             variable
more_frames:      u8        (0 = last, 1 = more)
```

`ColumnDesc`:
```
name:        u16 length + UTF-8 bytes
type_tag:    u8
nullable:    u8
```

Row encoding:
```
For each row:
  null_bitmap:   ceil(column_count / 8) bytes
  for each non-null column:
    value bytes (encoding per type_tag, with length prefix for variable types)
```

If `more_frames = 1`, the client expects more `SqlResult` frames with the same `request_id`.

## Query: Document

### `DocOp` (client → server, 0x30)

```
op_type:         u8       (1=insertOne, 2=insertMany, 3=find, 4=findOne,
                           5=updateOne, 6=updateMany, 7=deleteOne, 8=deleteMany)
collection:      u16 length + UTF-8 bytes
body:            variable, op-dependent
```

For `insertOne`: body is a single document (length-prefixed).
For `insertMany`: body is `u32 count + count × (u32 length + document)`.
For `find` and `findOne`: body is `{ query: document, options: document }`.
For `updateX`: body is `{ query: document, update: document, options: document }`.
For `deleteX`: body is `{ query: document, options: document }`.

### `DocResult` (server → client, 0x31)

```
status:          u8
affected:        u64
inserted_ids:    u32 count + count × ObjectId(12 bytes)    (for inserts)
doc_count:       u32
docs:            u32 doc_count + each: u32 length + document bytes
more_frames:     u8
```

Streamed in multiple frames for large result sets, same as SQL.

## Query: KV

### `KvOp` (client → server, 0x40)

```
op_type:        u8     (1=get, 2=put, 3=delete, 4=range, 5=scan)
namespace:      u16 length + UTF-8 bytes
[op-specific bodies follow]
```

`get`: `u16 key_len + key`.
`put`: `u16 key_len + key + u32 value_len + value`.
`delete`: `u16 key_len + key`.
`range`: `u16 start_len + start + u16 end_len + end + u32 max_results`.
`scan`: `u16 prefix_len + prefix + u32 max_results`.

### `KvResult` (server → client, 0x41)

```
status:        u8
op_type:       u8
[op-specific result follows]
```

`get`: `u8 found + (if found: u32 value_len + value)`.
`put`/`delete`: empty body (status indicates outcome).
`range`/`scan`: `u32 entry_count + entries: u16 key_len + key + u32 value_len + value` × entry_count + `more_frames: u8`.

## Cancellation

### `Cancel` (client → server, 0x50)

```
target_request_id: u32      (the in-flight request to abort)
```

Server tries to cancel the in-flight request; the executor sees the cancellation at the next operator boundary. The original request's response will be a `Cancelled` error.

## Notices

### `Notice` (server → client, 0x60)

Unsolicited (request_id = 0). Used for connection-level events.

```
severity:  u8       (0=info, 1=warning, 2=error)
code:      u32
message:   u16 length + UTF-8 bytes
```

Examples: `ServerShuttingDown`, `TxnReadOnly`, `IdempotencyConflict`.

## Errors

Any server response with `status != 0` includes an error trailer:

```
After the normal response body:
  error_code:    u32
  error_message: u16 length + UTF-8 bytes
  sqlstate:      5 bytes ASCII (e.g., "23505" for unique violation)
  detail:        u16 length + UTF-8 bytes (optional, may be empty)
  position:      u32 (character position in source SQL; 0 if not applicable)
```

### Error code ranges

```
0x0001 - 0x00FF   Protocol errors (framing, version, auth)
0x0100 - 0x01FF   Authentication / authorization
0x0200 - 0x02FF   Transaction errors (serialization, deadlock, timeout)
0x0300 - 0x03FF   Storage errors (out of space, I/O)
0x0400 - 0x04FF   Query errors (syntax, plan, type)
0x0500 - 0x05FF   Constraint violations
0x0600 - 0x06FF   Resource limits (memory, connections)
0xFF00 - 0xFFFF   Internal / unexpected
```

Specific codes are listed in the SDK documentation.

## Close

A client closes the connection by sending no further frames and closing the TCP connection. The server cleans up: aborts any active transaction, releases locks, closes resources.

The server may close the connection (with a `Notice` first) on shutdown, idle timeout, or detected protocol violation.

## Idle timeout

Default 600 seconds. If no frame is received in that window, the server sends a `Notice { severity=warning, code=IdleTimeout }` and closes the connection. Clients should send a no-op `Ping` (0x70) periodically if they need to keep connections alive longer.

### `Ping` (client → server, 0x70) / `Pong` (server → client, 0x71)

Both have empty bodies. Used as keep-alive.

## Concurrency on a connection

A client may have multiple in-flight requests. Each has a unique `request_id` chosen by the client. The server processes them concurrently to the extent possible (limited by transaction isolation: if all requests share one connection's transaction, they serialize through that transaction's lock state).

Responses arrive in arbitrary order; clients match by `request_id`.

## Versioning

The `protocol_version` in `Hello` is currently 1. A v2 with non-compatible changes will use a new value; the server may support multiple versions simultaneously during a deprecation window.

Backward-compatible additions (new message types, new fields gated on feature flags) do not bump the version.

## References

- ADR 0008 — binary protocol decision.
- `components/network-server.md` — server-side implementation.
- `specs/sdk-api.md` — high-level client surface.
- `specs/record-format.md` — type tag encodings.
