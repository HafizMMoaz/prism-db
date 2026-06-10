# @prismdb/client

A **pure-TypeScript** client for [PrismDB](../../README.md), speaking the binary
wire protocol directly over a TCP (or TLS) socket. No native addon, no build
toolchain beyond `tsc` — it runs anywhere Node does.

> Implements `docs/specs/wire-protocol.md`. The byte layouts are kept in lockstep
> with the Rust `prism-protocol` crate and validated end-to-end against `prismd`.

## Install

```bash
npm install @prismdb/client
```

Requires Node.js ≥ 20.

## Quick start

```ts
import { Client, Q } from "@prismdb/client";

const db = await Client.connect({
  host: "127.0.0.1",
  port: 4444,
  username: "admin",
  password: "admin",
});

// SQL
await db.sql("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)");
await db.sql("INSERT INTO users VALUES (1,'alice',30),(2,'bob',25)");
const res = await db.sql("SELECT name, age FROM users WHERE age >= 30 ORDER BY age");
console.log(res.rows); // [{ name: 'alice', age: 30n }]

// Key/value
await db.kv.put("sessions", "sid-1", "payload");
const v = await db.kv.get("sessions", "sid-1"); // Buffer | null

// Documents, with query operators
await db.doc.insertOne("people", { name: "carol", age: 41, city: "NYC" });
const adults = await db.doc.find("people", Q.and(Q.eq("city", "NYC"), Q.gt("age", 30)));

// A transaction is atomic across all three models
await db.begin();
await db.sql("INSERT INTO users VALUES (3,'dave',50)");
await db.kv.put("sessions", "sid-2", "tx");
await db.commit(); // or db.abort()

db.close();
```

## API

### `Client.connect(opts)`

`{ host?, port?, username?, password?, tls?, clientName?, clientVersion? }`.
Performs the `Hello`/`Auth` handshake. Omit `username` to skip authentication
(only useful against a server that doesn't require it). Pass `tls: true` (or Node
TLS options) for TLS.

### SQL — `db.sql(text, { params?, returnRows? })`

Returns `{ columns, rows, raw, affectedRows }`. `rows` are objects keyed by
column name; `raw` keeps the cells in column order. `affectedRows` is a `bigint`.

### KV — `db.kv`

`get(ns, key) → Buffer | null`, `put(ns, key, value)`, `delete(ns, key)`. Keys
and values are `string | Uint8Array`.

### Documents — `db.doc`

`insertOne`/`insertMany` (return the assigned `ObjectId`s), `find`/`findOne`,
`updateOne`/`updateMany` (the update document is an implicit `$set`),
`deleteOne`/`deleteMany`. Build filters with `Q`:

```ts
Q.all();
Q.eq("f", v); Q.ne; Q.gt; Q.lt; Q.gte; Q.lte;
Q.in("f", [a, b]); Q.nin("f", [a, b]);
Q.exists("f", true);
Q.and(a, b); Q.or(a, b); Q.not(a);
```

### Transactions — `db.begin(mode?)`, `db.commit({ idempotencyKey? })`, `db.abort()`

One `Client` is one server session, so calls between `begin()` and `commit()`
run in that transaction. `commit({ idempotencyKey })` makes a retried commit safe.

### Value mapping

JS → wire: `null`→Null, `boolean`→Bool, `bigint`→Int64, integer `number`→Int64,
fractional `number`→Double, `string`→Str, `Uint8Array`→Binary, `ObjectId`→ObjectId.
Use `int32(n)`, `float64(n)`, `timestamp(us)` to force a type. On decode, Int64
and Timestamp come back as `bigint`.

## Develop

```bash
npm install
npm run build       # tsc → dist/
npm test            # unit tests (no server needed)

# end-to-end against a running server:
cargo run -q -p prism-server --bin prismd -- run ./data 127.0.0.1:4455
PRISM_E2E_ADDR=127.0.0.1:4455 npm test
```

## Status / limitations

- Streamed (multi-frame) SQL/document results are not yet reassembled (the
  current server replies in a single frame).
- KV `range`/`scan` and wire update-operators (`$inc`, `$unset`) are follow-ups.
