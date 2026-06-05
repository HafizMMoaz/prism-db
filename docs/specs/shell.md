# Specification: Interactive Shell

**Status:** Accepted
**Last updated:** 2026-05-15
**Binary:** `prism-shell`

The interactive shell is a command-line client for human operators. It connects to a Prism server, accepts statements at a prompt, executes them, and renders results. It exists for operations, debugging, and live exploration; it is not the application-facing interface (that is the SDK).

## Invocation

```
prism-shell --host=localhost --port=4444 --user=app --database=prod
```

Flags:

```
--host             default localhost
--port             default 4444
--user             required
--password         optional; if omitted, prompt
--database         required
--tls              "off" | "on" | "verify" (default "verify" for non-localhost)
--ca               path to CA bundle for TLS
--ssl-cert         client cert for mTLS
--ssl-key          client key for mTLS
--mode             "sql" | "doc" | "kv" (default "sql"; switches via meta-command)
--output           "table" | "json" | "csv" (default "table")
--file             execute statements from file and exit
--command          execute one statement and exit
--quiet            suppress banner and progress
```

## Prompt and modes

The shell has three modes — `sql`, `doc`, `kv` — switched by meta-commands.

```
prism (sql)>     SQL mode prompt
prism (doc)>     Document mode prompt
prism (kv)>      KV mode prompt
```

### SQL mode

Multi-line input. A statement is terminated by a semicolon at the end of a line (with surrounding whitespace). Until a semicolon is seen, the prompt continues:

```
prism (sql)> SELECT id, name
       ...>   FROM users
       ...>   WHERE active = true;
```

Backslash escapes a newline if the user wants explicit multi-line within a statement on a single logical line.

### Document mode

Each line is a JSON document or operation. Commands are entered as JSON objects with a `_op` field, or via meta-commands:

```
prism (doc)> \use events
prism (doc)> { "type": "click", "user_id": 42 }
INSERTED 6543abc1...

prism (doc)> \find { "type": "click" }
{ "_id": "6543abc1...", "type": "click", "user_id": 42, ... }
```

### KV mode

Simple commands:

```
prism (kv)> \use sessions
prism (kv)> put alice "Alice's session blob"
OK
prism (kv)> get alice
"Alice's session blob"
```

## Meta-commands

Begin with `\`. Available in all modes:

```
\?                Show help
\connect <opts>   Reconnect with new options
\mode sql|doc|kv  Switch mode
\use <name>       Switch active collection / namespace (doc / kv modes)
\tables           List tables
\indexes <table>  List indexes for a table
\describe <obj>   Show schema / metadata
\d <table>        Alias for \describe
\collections      List document collections
\namespaces       List KV namespaces
\begin            Start an explicit transaction
\commit           Commit it
\abort            Abort it
\timing on|off    Show elapsed time per statement (default on)
\output <fmt>     Switch output format
\set <var>=<val>  Set a session variable (e.g., \set query_timeout=60s)
\watch <interval> Re-execute the last statement every <interval> seconds
\edit             Open the last statement in $EDITOR for editing
\source <file>    Execute statements from a file
\export <file>    Export the last result as CSV/JSON
\history          Show command history
\q, \quit, \exit  Disconnect and exit
```

## Output formats

### Table (default)

```
+----+--------+----------------+--------+
| id | name   | email          | active |
+----+--------+----------------+--------+
|  1 | Alice  | alice@a.com    | true   |
|  2 | Bob    | bob@b.com      | false  |
+----+--------+----------------+--------+
2 rows in 4ms
```

Column widths chosen to fit the terminal. Truncated cells end with `…`. Numerics right-aligned; strings left-aligned.

### JSON

```json
[
  {"id":1,"name":"Alice","email":"alice@a.com","active":true},
  {"id":2,"name":"Bob","email":"bob@b.com","active":false}
]
```

One JSON document per result set, on a single line in `--output=json`, pretty-printed in `--output=json-pretty`.

### CSV

RFC 4180 compliant. Headers on the first line.

## Transactions

By default, every statement runs in an implicit transaction (begin, run, commit). Errors abort.

Explicit transactions via `\begin`, `\commit`, `\abort`. While in an explicit transaction, the prompt indicates this:

```
prism (sql)* >       (asterisk = in transaction)
```

A connection drop during an explicit transaction is treated as `\abort` and the user is informed on reconnect.

## Error handling

Errors are rendered with code, location, and message:

```
ERROR  syntax_error  at position 14
  SELECT * FRM users;
                ^
  expected FROM
```

For execution-time errors, the SQLSTATE-like code, internal code, and message are shown. Optional `--show-detail` flag shows the full detail field.

## History and readline

The shell uses `rustyline` for line editing: up/down for history, Ctrl-R for reverse search, Ctrl-A/E for beginning/end of line, Tab for context-aware completion.

History is persisted to `~/.prism_history` (configurable via `PRISM_HISTORY` env var). Sensitive statements (`CREATE USER`, `GRANT`, anything containing a password) are not persisted.

Completion sources:
- SQL keywords.
- Table names, column names from the catalog (fetched lazily on first Tab).
- Collection and namespace names.
- Meta-command names.

## Scripting mode

`prism-shell --file=script.sql` runs every statement in the file and exits. Errors are written to stderr; on any error, the exit code is non-zero.

`prism-shell --command="SELECT count(*) FROM users"` runs one statement and exits.

In scripting mode, the prompt is suppressed and output defaults to `csv` (machine-friendly).

## Configuration file

`~/.prism/config.toml` is read on startup:

```toml
[default]
host = "primary.example.com"
port = 4444
user = "ops"
database = "prod"
tls = "verify"

[profiles.dev]
host = "localhost"
port = 4444
user = "dev"
tls = "off"
```

Selected via `--profile=dev` or the `PRISM_PROFILE` env var.

## Safety

The shell is for humans; it has safety features that the SDK does not:

- `DELETE FROM <table>` without `WHERE` prompts for confirmation.
- `DROP TABLE` prompts for confirmation.
- `\set destructive_ok=on` disables prompts for the session.
- In scripting mode, prompts are skipped and the operation proceeds (script authors are responsible).

## Output streaming

Long results stream as they arrive: rows appear incrementally. The pager (`less` or equivalent, set by `$PAGER`) is invoked automatically when output exceeds the terminal height, unless `--no-pager` is set.

## Profiling output

`\timing on` (default) shows per-statement elapsed time. `\timing detailed` shows server-side timing (parse, plan, execute) separately.

```
prism (sql)> SELECT * FROM big_table LIMIT 100;
[100 rows in 23ms — parse 0.2ms, plan 0.5ms, execute 22ms, fetch 0.1ms]
```

## Configuration

```
[shell]
default_format = "table"
default_mode = "sql"
history_file = "~/.prism_history"
history_size = 10000
timing = true
```

## References

- ADR 0008 — wire protocol the shell speaks.
- `specs/sdk-api.md` — the alternative for programmatic clients.
- `specs/wire-protocol.md` — the shell uses this directly.
- `rustyline` for the line editor.
