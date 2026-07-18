# Specification: Node.js SDK API

**Status:** Accepted (normative)
**Last updated:** 2026-05-15
**Package:** `@prism/client`
**Module type:** ESM and CommonJS, with `.d.ts` typings

This document specifies the public API surface of the Node.js client SDK. The SDK is the primary way applications interact with Prism. It is implemented as a `napi-rs` native module wrapping the Rust client library, giving zero-copy data transfer and good performance with full TypeScript typings.

## Installation

```bash
npm install @prism/client
```

Native binaries are prebuilt for `linux-x64`, `linux-arm64`, `darwin-arm64`. Source builds fall back via `cargo` if a prebuilt binary is not available for the target.

## Client

```typescript
import { Client } from '@prism/client';

const client = await Client.connect({
  host: 'localhost',
  port: 4444,
  user: 'app',
  password: process.env.PRISM_PASSWORD,
  database: 'production',
  tls: { ca: '/etc/ssl/prism-ca.crt' },
});
```

### `Client.connect(options): Promise<Client>`

Options:

```typescript
interface ConnectOptions {
  host: string;
  port?: number;            // default 4444
  user: string;
  password?: string;        // required unless tls.clientCert is set (mTLS)
  database: string;
  tls?: TlsOptions | false; // false = plaintext (default in dev, refused in prod)
  poolSize?: number;        // default 8; per-Client connection pool
  connectionTimeoutMs?: number;    // default 10000
  idleTimeoutMs?: number;          // default 300000
  applicationName?: string; // for server-side logging
}

interface TlsOptions {
  ca?: string | string[];        // path or PEM string
  clientCert?: string;           // for mTLS
  clientKey?: string;
  rejectUnauthorized?: boolean;  // default true
}
```

The `Client` maintains a pool of authenticated TCP connections. Pool checkout is implicit at each call.

### `client.close(): Promise<void>`

Drains in-flight requests, closes connections. After close, any further use throws.

### `client.healthcheck(): Promise<HealthStatus>`

Lightweight server liveness check. Returns server version and basic status. Does not consume a transaction.

## Surfaces

The client exposes three model surfaces and a transaction surface.

```typescript
client.sql      // SqlSurface
client.documents // DocumentSurface
client.kv        // KvSurface
client.tx        // factory for transactions
```

## SQL surface

```typescript
interface SqlSurface {
  // Execute a SQL statement. Auto-commits (implicit transaction).
  execute(sql: string, params?: Value[]): Promise<SqlResult>;

  // Execute a query, return rows as objects.
  query<T = Record<string, Value>>(sql: string, params?: Value[]): Promise<T[]>;

  // Execute a query, stream rows.
  stream<T = Record<string, Value>>(sql: string, params?: Value[]): AsyncIterableIterator<T>;

  // Prepare a statement for reuse (server-side plan caching).
  prepare(sql: string): Promise<PreparedStatement>;
}

interface SqlResult {
  affectedRows: number;
  rows?: Record<string, Value>[];   // present iff SELECT
  columnTypes?: ColumnDescriptor[];
}

interface PreparedStatement {
  execute(params?: Value[]): Promise<SqlResult>;
  close(): Promise<void>;
}

type Value = null | boolean | number | bigint | string | Date | Buffer | Value[] | { [k: string]: Value };
```

### Parameters

Parameters are positional, named `$1`, `$2`, etc. in the SQL string:

```typescript
await client.sql.execute(
  'INSERT INTO users (name, email) VALUES ($1, $2)',
  ['Alice', 'alice@example.com']
);
```

### Type mapping

| SQL type | JavaScript type |
|---|---|
| `BOOL` | `boolean` |
| `INT32` | `number` |
| `INT64` | `bigint` (`number` if `safeIntegers: false`) |
| `FLOAT32`, `FLOAT64` | `number` |
| `TEXT` | `string` |
| `BLOB` | `Buffer` |
| `TIMESTAMP` | `Date` |
| `NULL` | `null` |

`INT64` is `bigint` by default; this is correct but inconvenient. The client option `safeIntegers: false` returns `number`, accepting precision loss for values beyond 2^53.

## Document surface

```typescript
interface DocumentSurface {
  collection(name: string): DocumentCollection;
}

interface DocumentCollection {
  insertOne(doc: Document): Promise<ObjectId>;
  insertMany(docs: Document[]): Promise<ObjectId[]>;

  findOne(query: Query, options?: FindOptions): Promise<Document | null>;
  find(query: Query, options?: FindOptions): AsyncIterableIterator<Document>;
  countDocuments(query: Query): Promise<number>;

  updateOne(query: Query, update: Update): Promise<UpdateResult>;
  updateMany(query: Query, update: Update): Promise<UpdateResult>;

  deleteOne(query: Query): Promise<DeleteResult>;
  deleteMany(query: Query): Promise<DeleteResult>;

  createIndex(spec: IndexSpec): Promise<void>;
  dropIndex(name: string): Promise<void>;
  indexes(): Promise<IndexInfo[]>;
}

interface Document { [key: string]: Value; }
interface Query { [key: string]: Value | QueryOp; }    // MongoDB-subset
type Update = { $set?: Document; $unset?: Record<string, ''>; $inc?: Document; $push?: Document; $pull?: Document; };

interface FindOptions {
  limit?: number;
  skip?: number;
  sort?: Record<string, 1 | -1>;
  projection?: Record<string, 0 | 1>;
}

interface UpdateResult { matchedCount: number; modifiedCount: number; }
interface DeleteResult { deletedCount: number; }

interface IndexSpec {
  name?: string;
  keys: Record<string, 1 | -1 | 'hashed'>;
  unique?: boolean;
  sparse?: boolean;
}
```

### ObjectId

```typescript
import { ObjectId } from '@prism/client';

const id = ObjectId.generate();      // client-side generation
id.toString();                       // 24-char hex
ObjectId.from('507f1f77bcf86cd...');
```

## KV surface

```typescript
interface KvSurface {
  namespace(name: string): KvNamespace;
}

interface KvNamespace {
  get(key: Key): Promise<Buffer | null>;
  getString(key: Key): Promise<string | null>;
  put(key: Key, value: Buffer | string): Promise<void>;
  delete(key: Key): Promise<boolean>;

  putIfAbsent(key: Key, value: Buffer | string): Promise<boolean>;
  compareAndSet(key: Key, expected: Buffer | string, value: Buffer | string): Promise<boolean>;

  // btree namespaces only:
  range(start: Key, end: Key, options?: RangeOptions): AsyncIterableIterator<[Buffer, Buffer]>;
  scan(prefix: Key, options?: RangeOptions): AsyncIterableIterator<[Buffer, Buffer]>;
}

type Key = Buffer | string;     // strings are encoded as UTF-8

interface RangeOptions {
  limit?: number;
  reverse?: boolean;        // not v1
}
```

## Transactions

```typescript
interface TxnFactory {
  begin(options?: TxnOptions): Promise<Transaction>;
  run<T>(fn: (tx: Transaction) => Promise<T>, options?: TxnOptions): Promise<T>;
}

interface TxnOptions {
  readOnly?: boolean;
  timeoutMs?: number;       // default 30000
  retryOnSerializationFailure?: boolean;   // default true for tx.run
  maxRetries?: number;      // default 3
}

interface Transaction {
  sql: SqlSurface;          // scoped to this transaction
  documents: DocumentSurface;
  kv: KvSurface;

  commit(): Promise<void>;
  abort(): Promise<void>;
}
```

### `client.tx.begin`

Explicit transaction control:

```typescript
const tx = await client.tx.begin();
try {
  await tx.sql.execute('INSERT INTO users ...');
  await tx.documents.collection('events').insertOne({ ... });
  await tx.kv.namespace('cache').put('key', 'value');
  await tx.commit();
} catch (e) {
  await tx.abort();
  throw e;
}
```

The transaction holds a single connection from the pool. Other code using `client` (without `tx`) runs on different connections.

### `client.tx.run`

Convenience wrapper with automatic retry on serialization failure:

```typescript
const result = await client.tx.run(async (tx) => {
  const user = await tx.sql.query('SELECT * FROM users WHERE id = $1', [id]);
  await tx.sql.execute('UPDATE users SET visits = visits + 1 WHERE id = $1', [id]);
  return user[0];
});
```

If the transaction body throws, the SDK calls `abort` and propagates the error. If the error is a serialization failure and `retryOnSerializationFailure` is true, the SDK retries up to `maxRetries` times with exponential backoff.

The body **must be idempotent** with respect to non-database side effects: if the body sends an email, retries will resend.

## Error handling

```typescript
import { PrismError, ErrorCode } from '@prism/client';

try {
  await client.sql.execute('SELECT ...');
} catch (e) {
  if (e instanceof PrismError) {
    console.error(e.code, e.sqlState, e.message);
  }
}
```

`ErrorCode` enum:

```typescript
enum ErrorCode {
  // Protocol
  ProtocolViolation = 0x0001,
  ConnectionClosed  = 0x0002,
  // Auth
  AuthenticationFailed = 0x0101,
  Unauthorized         = 0x0102,
  // Transactions
  SerializationFailure = 0x0201,
  Deadlock             = 0x0202,
  TransactionTimeout   = 0x0203,
  TransactionAborted   = 0x0204,
  // Storage
  IoError              = 0x0301,
  OutOfDiskSpace       = 0x0302,
  // Query
  SyntaxError          = 0x0401,
  TypeError            = 0x0402,
  ObjectNotFound       = 0x0403,
  ObjectAlreadyExists  = 0x0404,
  // Constraint
  UniqueViolation      = 0x0501,
  CheckViolation       = 0x0502,
  // Resource
  OutOfMemory          = 0x0601,
  TooManyConnections   = 0x0602,
  QueryTooComplex      = 0x0603,
  // Internal
  InternalError        = 0xFF01,
}
```

`PrismError` instances have:
```typescript
class PrismError extends Error {
  code: ErrorCode;
  sqlState: string;
  detail?: string;
  position?: number;        // character offset in SQL source
}
```

## Idempotency keys

```typescript
await client.tx.run(async (tx) => { ... }, { idempotencyKey: 'abc-123' });
```

If the SDK detects a connection failure between the commit fsync and the client receiving the ack, it retries with the same idempotency key; the server returns the cached response of the original commit.

Keys are scoped to the user and expire after 24 hours.

## Logging and tracing

The SDK emits OpenTelemetry spans for every request when an OTEL SDK is initialized in the process. Spans:

- `prism.sql.execute`
- `prism.sql.query`
- `prism.documents.insertOne`
- `prism.kv.get`
- ... and so on.

Span attributes include `prism.operation`, `prism.collection`, `prism.affected_rows`, `prism.server_version`. The SQL text is **not** included by default (sensitive data); enable via `client.options.captureSqlText = true`.

## Examples

### Simple insert and read

```typescript
const client = await Client.connect({ host, user, password, database });

await client.sql.execute(
  'CREATE TABLE IF NOT EXISTS posts (id INT PRIMARY KEY, title TEXT, body TEXT)'
);

await client.sql.execute(
  'INSERT INTO posts (id, title, body) VALUES ($1, $2, $3)',
  [1, 'hello', 'world']
);

const posts = await client.sql.query('SELECT * FROM posts');
console.log(posts);
```

### Cross-model transaction

```typescript
await client.tx.run(async (tx) => {
  await tx.sql.execute(
    'UPDATE accounts SET balance = balance - $1 WHERE id = $2',
    [100n, 'A']
  );
  await tx.sql.execute(
    'UPDATE accounts SET balance = balance + $1 WHERE id = $2',
    [100n, 'B']
  );
  await tx.documents.collection('audit').insertOne({
    type: 'transfer', from: 'A', to: 'B', amount: 100,
    at: new Date(),
  });
  await tx.kv.namespace('limits').put(`rate:A:${today()}`, '1');
});
```

A crash anywhere inside this block leaves either all four effects applied or none. That is the cross-model atomicity guarantee.

### Streaming a large result

```typescript
for await (const row of client.sql.stream('SELECT * FROM events WHERE day = $1', [day])) {
  process(row);
}
```

The stream yields rows as the server sends them; memory usage is bounded.

## Compatibility

- Node.js 18, 20, 22.
- ESM-first; CJS supported for legacy consumers.
- TypeScript types are first-class; the SDK is fully typed including the type-level inference of column types from prepared statements (limited).

## Versioning

The SDK follows semver. Breaking changes only in major versions; minor versions add features. The wire protocol version is independent; the SDK supports the protocol versions the server supports, negotiating at connect.

## References

- ADR 0008 - protocol design.
- ADR 0009 - napi-rs decision.
- `specs/wire-protocol.md` - the protocol this SDK speaks.
- napi-rs: https://napi.rs
