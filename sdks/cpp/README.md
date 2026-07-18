# prism (C++)

A modern **C++17** client for [PrismDB](https://github.com/HafizMMoaz/prism-db), speaking the binary
wire protocol directly over a TCP socket. The value model and codec are
header-only; the transport lives in one `.cpp`. No third-party dependencies.
Cross-platform: Winsock on Windows, BSD sockets elsewhere.

> Implements `docs/specs/wire-protocol.md`. The byte layouts are kept in lockstep
> with the Rust `prism-protocol` crate and the reference Node SDK.

## Build

```bash
make            # builds libprism.a
make test       # builds & runs the header-only codec tests
make example    # builds the quickstart binary
```

Or compile directly:

```bash
c++ -std=c++17 -Iinclude my_app.cpp src/prism.cpp -o my_app           # Linux/macOS
g++ -std=c++17 -Iinclude my_app.cpp src/prism.cpp -lws2_32 -o app     # Windows/MinGW
```

## Quick start

```cpp
#include "prism/prism.hpp"
#include <iostream>

int main() {
    prism::Options opts;
    opts.username = "admin";
    opts.password = "admin";
    prism::Client db = prism::Client::connect(opts);

    // SQL ($1, $2, ... positional params)
    db.sql("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)");
    db.sql("INSERT INTO users VALUES ($1, $2, $3)", {std::int64_t(1), "alice", std::int64_t(30)});
    auto res = db.sql("SELECT name, age FROM users WHERE age >= $1", {std::int64_t(18)});
    int nameCol = res.column("name");
    for (const auto& row : res.rows) std::cout << row[nameCol].asString() << "\n";

    // Key/value
    db.kvPut("sessions", "sid-1", "payload");
    auto v = db.kvGet("sessions", "sid-1");      // std::optional<std::vector<uint8_t>>

    // Documents, with query operators
    db.insertOne("people", prism::Document{{"name", "carol"}, {"age", std::int64_t(41)}, {"city", "NYC"}});
    auto adults = db.find("people",
        prism::Query::and_(prism::Query::eq("city", "NYC"), prism::Query::gt("age", std::int64_t(30))));

    // A transaction is atomic across all three models
    db.begin();
    db.sql("INSERT INTO users VALUES (2,'dave',50)");
    db.kvPut("sessions", "sid-2", "tx");
    db.commit();                                  // or db.abort()
}
```

## API

- **`prism::Value`** - a `std::variant`-backed value. Construct implicitly from
  `bool`, integers (default to **Int64**), `double`, `std::string`/`const char*`,
  `std::vector<uint8_t>` (Binary), `prism::Int32{n}`, `prism::Timestamp{us}`,
  `prism::ObjectId`, or `nullptr` (Null). Read with `asBool/asInt64/asInt32/
  asDouble/asString/asBytes/asObjectId` (throw on mismatch).
- **SQL** - `db.sql(text, params)` → `SqlResult{columns, rows, affectedRows}`;
  `result.column("name")` gives a column index.
- **KV** - `kvGet` (→ `std::optional`), `kvPut`, `kvDelete`.
- **Documents** - `insertOne`/`insertMany`, `find`/`findOne`, `count`,
  `updateOne`/`updateMany`, `deleteOne`/`deleteMany`. Build filters with
  `prism::Query` (`all/eq/ne/gt/lt/gte/lte/in/nin/exists/and_/or_/not_`) and
  updates with `prism::Update{}.set(...).inc(...).unset(...)`.
- **Transactions** - `begin(readOnly=false)`, `commit(idempotencyKey=0)`, `abort()`.
- **Errors** - `prism::ServerError` (carries `info.code`, `info.sqlstate`,
  `info.message`, `info.detail`, `info.position`) and `prism::ProtocolError`,
  both deriving from `prism::Error`.

## Status / limitations

- **TLS is not yet supported** by the C++ core (no crypto dependency). Use a
  TLS-terminating proxy until this lands.
- Streamed (multi-frame) SQL/document results are not yet reassembled.
- KV `range`/`scan` are follow-ups.
- The client is synchronous and single-connection (movable, non-copyable); use
  one `Client` per thread.
