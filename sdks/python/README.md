# prismdb (Python)

A **pure-Python** client for [PrismDB](https://github.com/HafizMMoaz/prism-db), speaking the binary
wire protocol directly over a TCP (or TLS) socket. No native extension, no build
toolchain — it runs anywhere CPython does.

> Implements `docs/specs/wire-protocol.md`. The byte layouts are kept in lockstep
> with the Rust `prism-protocol` crate and the reference Node SDK.

## Install

```bash
pip install prismdb
```

Requires Python ≥ 3.8. No dependencies.

## Quick start

```python
from prismdb import Client, Q, U

db = Client.connect(host="127.0.0.1", port=4444, username="admin", password="admin")
with db:
    # SQL
    db.sql("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)")
    db.sql("INSERT INTO users VALUES (1,'alice',30),(2,'bob',25)")
    res = db.sql("SELECT name, age FROM users WHERE age >= 30 ORDER BY age")
    print(res.rows)  # [{'name': 'alice', 'age': 30}]

    # Key/value
    db.kv.put("sessions", "sid-1", "payload")
    v = db.kv.get("sessions", "sid-1")          # bytes | None

    # Documents, with query operators
    db.doc.insert_one("people", {"name": "carol", "age": 41, "city": "NYC"})
    adults = db.doc.find("people", Q.and_(Q.eq("city", "NYC"), Q.gt("age", 30)))

    # A transaction is atomic across all three models
    db.begin()
    db.sql("INSERT INTO users VALUES (3,'dave',50)")
    db.kv.put("sessions", "sid-2", "tx")
    db.commit()                                  # or db.abort()
```

`Client` is a context manager; leaving the `with` block closes the connection.

## API

### `Client.connect(host="127.0.0.1", port=4444, *, username=None, password=None, database=None, tls=None, ...)`

Performs the `Hello`/`Auth` handshake. Omit `username` to skip authentication
(only useful against a server that doesn't require it). Pass `tls=True` (or an
`ssl.SSLContext`) for TLS. On a multi-database server, pass `database=` to select
it at connect; otherwise run `db.sql("USE <name>")` yourself.

### SQL — `db.sql(text, params=None, *, return_rows=True)`

Returns a `SqlResult` with `.columns`, `.rows` (list of dicts keyed by column
name), `.raw` (cells in column order), and `.affected_rows` (int).

### KV — `db.kv`

`get(ns, key) -> bytes | None`, `put(ns, key, value)`, `delete(ns, key)`. Keys
and values are `str` (UTF-8) or `bytes`.

### Documents — `db.doc`

`insert_one` / `insert_many` (return the assigned `ObjectId`s), `find` / `find_one`,
`count`, `update_one` / `update_many`, `delete_one` / `delete_many`. Build filters
with `Q` and updates with `U`:

```python
Q.all()
Q.eq("f", v); Q.ne; Q.gt; Q.lt; Q.gte; Q.lte
Q.in_("f", [a, b]); Q.nin("f", [a, b])
Q.exists("f", True)
Q.and_(a, b); Q.or_(a, b); Q.not_(a)

db.doc.update_one("people", Q.eq("name", "carol"), [
    U.set("city", "Boston"),
    U.inc("age", 1),
    U.unset("temp"),
])
```

### Transactions — `db.begin(mode="read_write")`, `db.commit(idempotency_key=0)`, `db.abort()`

One `Client` is one server session, so calls between `begin()` and `commit()`
run in that transaction. `commit(idempotency_key=...)` makes a retried commit safe.

### Value mapping

Python → wire: `None`→Null, `bool`→Bool, `int`→Int64, `float`→Double,
`str`→Str, `bytes`→Binary, `datetime`→Timestamp, `ObjectId`→ObjectId. Use
`int32(n)`, `float64(n)`, `timestamp(us)` to force a type. On decode, Int64 and
Timestamp come back as `int`.

## Develop

```bash
python -m unittest discover -s tests   # unit tests (no server needed)

# end-to-end against a running server:
prismd run ./data 127.0.0.1:4444
PRISM_HOST=127.0.0.1 PRISM_PORT=4444 python examples/quickstart.py
```

## Status / limitations

- Streamed (multi-frame) SQL/document results are not yet reassembled (the
  current server replies in a single frame).
- KV `range`/`scan` are follow-ups.
- The client is synchronous; one `Client` owns one connection. Use a `Client`
  per thread, or one per concurrent transaction.
