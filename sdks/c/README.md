# prism (C)

A **pure-C** client for [PrismDB](https://github.com/HafizMMoaz/prism-db), speaking the binary wire
protocol directly over a TCP socket. Single header (`include/prism.h`) plus a
single source file (`src/prism.c`), no third-party dependencies — drop it into
any C99/C11 project. Cross-platform: Winsock on Windows, BSD sockets elsewhere.

> Implements `docs/specs/wire-protocol.md`. The byte layouts are kept in lockstep
> with the Rust `prism-protocol` crate and the reference Node SDK.

## Build

```bash
make            # builds libprism.a
make test       # builds & runs the no-server codec tests
make example    # builds the quickstart binary
```

Or vendor the two files directly:

```bash
cc -std=c11 -Iinclude my_app.c src/prism.c -o my_app          # Linux/macOS
gcc -std=c11 -Iinclude my_app.c src/prism.c -lws2_32 -o app   # Windows/MinGW
```

## Quick start

```c
#include "prism.h"
#include <stdio.h>

int main(void) {
    prism_options o = prism_options_default();
    o.username = "admin";
    o.password = "admin";

    prism_client *c = NULL;
    if (prism_connect(&o, &c) != PRISM_OK) return 1;

    prism_result *r = NULL;
    prism_sql(c, "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)", NULL, 0, &r);
    prism_result_free(r);

    /* parameterised SQL ($1, $2, ...) */
    prism_value params[2] = { prism_i64(1), prism_str("alice") };
    prism_sql(c, "INSERT INTO users VALUES ($1, $2)", params, 2, &r);
    prism_result_free(r);

    prism_sql(c, "SELECT id, name FROM users", NULL, 0, &r);
    for (size_t row = 0; row < prism_result_rows(r); row++) {
        prism_value name = prism_result_get(r, row, 1);
        printf("%.*s\n", (int)name.as.str.len, name.as.str.ptr);
    }
    prism_result_free(r);

    prism_client_free(c);
    return 0;
}
```

## API shape

- **Errors.** Every server call returns a `prism_status` (`PRISM_OK == 0`). On
  `PRISM_ERR_SERVER`, `prism_last_error(c)` returns the structured trailer
  (`code`, `sqlstate`, `message`, `detail`, `position`).
- **Values.** Construct with `prism_null/bool/i32/i64/f64/str/strn/bytes/
  timestamp/objectid`. Decoded values borrow memory owned by their result/doc
  container and are valid until it is freed.
- **SQL.** `prism_sql()` → a `prism_result` (`prism_result_rows/cols/col_name/
  get/affected`, freed with `prism_result_free`).
- **KV.** `prism_kv_get/put/delete`. A found `get` value is a malloc'd buffer
  the caller frees with `prism_free`.
- **Documents.** Build with `prism_doc_new/set`; query with the `prism_q_*`
  builders and update with `prism_update_*`. `prism_doc_insert_one/find/find_one/
  count/update_one/update_many/delete_one/delete_many`. Free result arrays with
  `prism_docs_free`. `prism_q_and/or/not` take ownership of their sub-queries.
- **Transactions.** `prism_begin/commit/abort`.
- **ObjectId.** `prism_objectid_to_hex` / `prism_objectid_from_hex`.

## Value mapping

C → wire: `prism_i64`→Int64 (the default integer), `prism_i32`→Int32,
`prism_f64`→Double, `prism_bool`→Bool, `prism_str`→String, `prism_bytes`→Binary,
`prism_timestamp`→Timestamp (µs since the epoch), `prism_objectid`→ObjectId,
`prism_null`→Null. Documents reject Binary fields (matches the engine).

## Status / limitations

- **TLS is not yet supported** by the C core (it has no crypto dependency);
  `use_tls = 1` returns `PRISM_ERR_USAGE`. Use a TLS-terminating proxy, or one of
  the other SDKs, until this lands.
- Streamed (multi-frame) SQL/document results are not yet reassembled.
- KV `range`/`scan` are follow-ups.
- The client is synchronous and single-connection; use one `prism_client` per
  thread.
