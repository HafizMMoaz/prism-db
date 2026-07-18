# PrismDb.Client (.NET / C#)

A **pure-C#** client for [PrismDB](https://github.com/HafizMMoaz/prism-db), speaking the binary wire
protocol directly over a TCP (or TLS) socket. No native dependency - it targets
`netstandard2.0`, so it runs on .NET Framework 4.6.1+, .NET Core, and .NET 5+.

> Implements `docs/specs/wire-protocol.md`. The byte layouts are kept in lockstep
> with the Rust `prism-protocol` crate and the reference Node SDK.

## Install

```bash
dotnet add package PrismDb.Client
```

## Quick start

```csharp
using PrismDb;

using var db = Client.Connect(host: "127.0.0.1", port: 4444, username: "admin", password: "admin");

// SQL
db.Sql("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)");
db.Sql("INSERT INTO users VALUES (1,'alice',30),(2,'bob',25)");
var res = db.Sql("SELECT name, age FROM users WHERE age >= 30 ORDER BY age");
foreach (var row in res.Rows) Console.WriteLine($"{row["name"]} {row["age"]}");

// Key/value
db.Kv.Put("sessions", "sid-1", "payload");
string? v = db.Kv.GetString("sessions", "sid-1");

// Documents, with query operators
db.Doc.InsertOne("people", new Document { ["name"] = "carol", ["age"] = 41L, ["city"] = "NYC" });
var adults = db.Doc.Find("people", Q.And(Q.Eq("city", "NYC"), Q.Gt("age", 30L)));

// A transaction is atomic across all three models
db.Begin();
db.Sql("INSERT INTO users VALUES (3,'dave',50)");
db.Kv.Put("sessions", "sid-2", "tx");
db.Commit();   // or db.Abort()
```

`Client` is `IDisposable`; a `using` block closes the connection.

## API

### `Client.Connect(...)`

Two overloads: a positional convenience (`host`, `port`, `username`, `password`,
`database`, `tls`) and a `ConnectOptions` object for full control (TLS server
name, connect timeout, notice handler, client name/version). Omit `username` to
skip authentication. Pass `tls: true` to validate the server certificate against
the OS trust store.

### SQL - `db.Sql(text, parameters = null, returnRows = true)`

Returns a `SqlResult` with `Columns`, `Rows` (dictionaries keyed by column name),
`Raw` (cells in column order), and `AffectedRows` (`ulong`). Parameters are
positional, `$1`, `$2`, … in the SQL text.

### KV - `db.Kv`

`Get(ns, key) → byte[]?`, `GetString(ns, key) → string?`, `Put(ns, key, value)`,
`Delete(ns, key)`. Keys/values are `string` (UTF-8) or `byte[]`.

### Documents - `db.Doc`

`InsertOne` / `InsertMany` (return the assigned `ObjectId`s), `Find` / `FindOne`,
`Count`, `UpdateOne` / `UpdateMany`, `DeleteOne` / `DeleteMany`. Build filters with
`Q` and updates with `U`:

```csharp
Q.All();
Q.Eq("f", v); Q.Ne; Q.Gt; Q.Lt; Q.Gte; Q.Lte;
Q.In("f", new object?[] { a, b }); Q.Nin(...);
Q.Exists("f", true);
Q.And(a, b); Q.Or(a, b); Q.Not(a);

db.Doc.UpdateOne("people", Q.Eq("name", "carol"), new[] {
    U.Set("city", "Boston"),
    U.Inc("age", 1),
    U.Unset("temp"),
});
```

### Transactions - `db.Begin(readOnly = false)`, `db.Commit(...)`, `db.Abort()`

One `Client` is one server session, so calls between `Begin()` and `Commit()` run
in that transaction.

### Value mapping

CLR → wire: `null`→Null, `bool`→Bool, integer types (`int`/`long`/…)→Int64,
`float`/`double`→Double, `string`→Str, `byte[]`→Binary, `DateTime`/`DateTimeOffset`
→Timestamp, `ObjectId`→ObjectId. Use `Prism.Int32(n)`, `Prism.Float64(n)`,
`Prism.Timestamp(us)` to force a wire type. On decode, Int64 and Timestamp come
back as `long`.

## Develop

```bash
dotnet run --project tests/PrismDb.Tests   # unit tests (no server needed)

# end-to-end against a running server:
prismd run ./data 127.0.0.1:4444
dotnet run --project examples/Quickstart
```

## Status / limitations

- Streamed (multi-frame) SQL/document results are not yet reassembled.
- KV `range`/`scan` are follow-ups.
- The client is synchronous; one `Client` owns one connection.
