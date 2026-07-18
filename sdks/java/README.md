# prism-client (Java)

A **pure-Java** client for [PrismDB](https://github.com/HafizMMoaz/prism-db), speaking the binary wire
protocol directly over a TCP (or TLS) socket. No native dependency, no JNI - just
the JDK. Requires Java 11+.

> Implements `docs/specs/wire-protocol.md`. The byte layouts are kept in lockstep
> with the Rust `prism-protocol` crate and the reference Node SDK.

## Install (Maven)

```xml
<dependency>
  <groupId>dev.prism</groupId>
  <artifactId>prism-client</artifactId>
  <version>0.1.0</version>
</dependency>
```

## Quick start

```java
import dev.prism.client.*;
import java.util.*;

try (Client db = Client.builder().username("admin").password("admin").connect()) {
    // SQL ($1, $2, ... positional params)
    db.sql("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)");
    db.sql("INSERT INTO users VALUES ($1, $2, $3)", List.of(1L, "alice", 30L));
    SqlResult res = db.sql("SELECT name, age FROM users WHERE age >= $1", List.of(18L));
    for (Map<String, Object> row : res.rows) System.out.println(row.get("name"));

    // Key/value
    db.kv.put("sessions", "sid-1", "payload");
    byte[] v = db.kv.get("sessions", "sid-1");          // null if absent

    // Documents, with query operators
    db.doc.insertOne("people", new Document().set("name", "carol").set("age", 41L).set("city", "NYC"));
    List<Document> adults = db.doc.find("people", Q.and(Q.eq("city", "NYC"), Q.gt("age", 30L)));

    // A transaction is atomic across all three models
    db.begin();
    db.sql("INSERT INTO users VALUES (2,'dave',50)");
    db.kv.put("sessions", "sid-2", "tx");
    db.commit();                                         // or db.abort()
}
```

`Client` is `AutoCloseable`; a try-with-resources block closes the connection.

## API

### `Client.builder()...connect()`

Fluent options: `host`, `port`, `username` (omit to skip auth), `password`,
`database`, `tls(true)`, `connectTimeoutMs`, `clientName`, `clientVersion`. There
is also `Client.connect(host, port, username, password)`.

### SQL - `db.sql(text)` / `db.sql(text, params)`

`params` is any `List<?>`. Returns a `SqlResult` with `columns`, `rows` (maps keyed
by column name), `raw` (cells in column order), and `affectedRows`.

### KV - `db.kv`

`get(ns, key) → byte[]` (null if absent), `getString(...)`, `put(ns, key, value)`,
`delete(ns, key)`. Keys/values are `String` (UTF-8) or `byte[]`.

### Documents - `db.doc`

`insertOne` / `insertMany` (return the assigned `ObjectId`s), `find` / `findOne`,
`count`, `updateOne` / `updateMany`, `deleteOne` / `deleteMany`. Build filters with
`Q` and updates with `U`:

```java
Q.all();
Q.eq("f", v); Q.ne; Q.gt; Q.lt; Q.gte; Q.lte;
Q.in("f", List.of(a, b)); Q.nin("f", List.of(a, b));
Q.exists("f", true);
Q.and(a, b); Q.or(a, b); Q.not(a);

db.doc.updateOne("people", Q.eq("name", "carol"),
    List.of(U.set("city", "Boston"), U.inc("age", 1), U.unset("temp")));
```

### Transactions - `db.begin()` / `db.begin(readOnly)`, `db.commit()` / `db.commit(key)`, `db.abort()`

One `Client` is one server session, so calls between `begin()` and `commit()` run
in that transaction.

### Value mapping

Java → wire: `null`→Null, `Boolean`→Bool, integer boxes (`Integer`/`Long`/…)→Int64,
`Float`/`Double`→Double, `String`→Str, `byte[]`→Binary, `java.time.Instant`→Timestamp,
`ObjectId`→ObjectId. Use `Values.int32(n)`, `Values.float64(n)`,
`Values.timestamp(us)` to force a wire type. On decode, Int64 and Timestamp come
back as `Long`.

### Errors

`ServerException` (with `code`, `sqlstate`, `detail`, `position`) for server-side
failures and `ProtocolException` for decode failures, both extending
`PrismException` (unchecked).

## Develop

```bash
mvn test            # unit tests (no server needed; downloads JUnit on first run)

# end-to-end against a running server:
prismd run ./data 127.0.0.1:4444
mvn -q compile
java -cp target/classes dev.prism.examples.Quickstart
```

## Status / limitations

- TLS uses the JVM default trust store (`tls(true)`).
- Streamed (multi-frame) SQL/document results are not yet reassembled.
- KV `range`/`scan` are follow-ups.
- The client is synchronous and single-connection; use one `Client` per thread.
