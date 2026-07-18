![Logo](./logo.png)

# PrismDB

A single-node, multi-model database engine: relational tables (SQL), JSON-like
documents, and ordered key-value pairs - all on one storage engine, sharing one
buffer pool, one write-ahead log, and one transaction manager. **A single
transaction can mutate rows, documents, and KV pairs atomically.**

[![CI](https://github.com/HafizMMoaz/prism-db/actions/workflows/ci.yml/badge.svg)](https://github.com/HafizMMoaz/prism-db/actions/workflows/ci.yml)
&nbsp;License: Apache-2.0 &nbsp;·&nbsp; Rust 1.85+ &nbsp;·&nbsp; Linux · macOS · Windows

This is not a wrapper around three databases. It is one engine with three access
methods on top of a unified, WAL-logged, MVCC record store.

## Install

PrismDB ships a single installer per platform. One install gives you the server
(`prismd`), the interactive client (`prism-shell`), and the `prism-fsck` /
`prism-dump` utilities.

**Linux / macOS** - shell installer:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/HafizMMoaz/prism-db/releases/latest/download/prismdb-installer.sh | sh
```

**macOS / Linux** - Homebrew:

```sh
brew install HafizMMoaz/prism/prismdb
```

**Windows** - PowerShell installer:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/HafizMMoaz/prism-db/releases/latest/download/prismdb-installer.ps1 | iex"
```

…or download the `.msi` from the [latest release](https://github.com/HafizMMoaz/prism-db/releases/latest).

**Debian / Ubuntu / Fedora / RHEL** - the `apt`/`dnf` package installs the
binaries **and** starts `prismd` as a systemd service:

```sh
# Debian / Ubuntu
curl -fsSL https://hafizmmoaz.github.io/prism-db/prismdb-archive-keyring.asc | sudo gpg --dearmor -o /usr/share/keyrings/prismdb.gpg
echo "deb [signed-by=/usr/share/keyrings/prismdb.gpg] https://hafizmmoaz.github.io/prism-db/deb ./" | sudo tee /etc/apt/sources.list.d/prismdb.list
sudo apt update && sudo apt install prismdb
```

(Fedora/RHEL repo and direct `.deb`/`.rpm` downloads in
[docs/operations/install.md](docs/operations/install.md).)

Prefer to build it yourself? `cargo install --git https://github.com/HafizMMoaz/prism-db prismdb`.

See [docs/operations/install.md](docs/operations/install.md) for running `prismd`
as a service (systemd / launchd / Windows) and for the data-directory layout.

## Quick start

```sh
prismd init                       # create the data directory
prismd run --bind 127.0.0.1:4444  # start the server (durable: fsync on commit)
```

In another terminal, connect with the shell (default account `admin` / `admin`
- change it before exposing the server):

```sh
prism-shell 127.0.0.1:4444 --user admin --password admin
```

```sql
CREATE DATABASE shop;
USE shop;

-- Relational
CREATE TABLE items (id BIGINT PRIMARY KEY, name TEXT, price BIGINT);
INSERT INTO items VALUES (1, 'book', 1200), (2, 'pen', 150);
SELECT name, price FROM items WHERE price > 200 ORDER BY price;

-- and, in the same session, documents (\doc) and key-value (\kv).
```

All three models share one transaction: `\begin`, mutate across models, `\commit`
- it is atomic and durable, or it is nothing.

## What's inside

- **Three models, one engine.** SQL tables, documents, and KV pairs over a shared
  slotted-page store with a single WAL and MVCC snapshot isolation.
- **ACID across models.** One transaction spans all three; commit is atomic and
  crash-safe (WAL + checkpoints + redo recovery).
- **Durable B-tree indexes** for every model (SQL primary keys, document `_id`,
  KV keys), all WAL-logged.
- **SQL surface:** `CREATE/ALTER/DROP TABLE`, `CREATE [UNIQUE] INDEX` (single- or
  multi-column secondary indexes), `INSERT … VALUES`/`INSERT … SELECT`,
  `UPDATE/DELETE`, and `SELECT` with
  `WHERE`/`GROUP BY … HAVING`/`ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT`, combinable
  with `UNION`/`INTERSECT`/`EXCEPT`. All join kinds (`INNER`/`LEFT`/`RIGHT`/
  `FULL OUTER`/`CROSS`, self-joins, and `ON`/`USING`/`NATURAL`), aggregates
  (`COUNT/SUM/AVG/MIN/MAX`), subqueries (scalar, `IN`, `EXISTS` - correlated in
  `WHERE`), primary-key equality **and range** index seeks, `CASE`, `CAST`, and
  date/string/numeric scalar functions over `BOOL`/`BIGINT`/`DOUBLE`/`TIMESTAMP`/
  `TEXT`.
- **Multi-tenant server.** Many named databases under one instance, scrypt-hashed
  accounts, role-based access with **per-database grants**, TLS, connection limits,
  idempotent commits, structured audit logging.
- **Clients:** the `prism-shell` REPL, a typed async Rust client, and pure
  client SDKs for **seven languages** (Node, Python, Java, .NET, C++, C, PHP) -
  see [Client SDKs](#client-sdks).
- **Operations:** offline integrity checker (`prism-fsck`), logical export/import
  (`prism-dump`), and a workload benchmark harness.

## Client SDKs

Official client libraries live in [`sdks/`](sdks/). Every SDK is a **pure
implementation of the [binary wire protocol](docs/specs/wire-protocol.md)** - no
native add-ons, nothing to compile on the user's machine beyond the language's
own toolchain. They share one surface: SQL (with `$1` params), documents (with
`Q`/`U` builders), key-value, and cross-model transactions.

| Language | Package | Install |
|----------|---------|---------|
| Node / TypeScript | [`@prismdb/client`](https://www.npmjs.com/package/@prismdb/client) | `npm install @prismdb/client` |
| Python | [`prismdb`](sdks/python) | `pip install prismdb` |
| Java | [`dev.prism:prism-client`](sdks/java) | Maven dependency |
| C# / .NET | [`PrismDb.Client`](sdks/dotnet) | `dotnet add package PrismDb.Client` |
| C++ (17) | [header + `prism.cpp`](sdks/cpp) | vendor or `make` |
| C (99/11) | [`prism.h` + `prism.c`](sdks/c) | vendor or `make` |
| PHP | [`prismdb/client`](sdks/php) | `composer require prismdb/client` |

See [sdks/README.md](sdks/README.md) for the common surface and per-language guides.

## Building from source

Requires Rust 1.85+.

```sh
cargo build --release        # binaries in target/release/
cargo test --workspace       # the full suite
```

See [docs/operations/build-and-dev.md](docs/operations/build-and-dev.md).

## Documentation

The design corpus lives in [`docs/`](docs/). Good entry points:

- [Executive summary](docs/overview/executive-summary.md) - one page
- [System architecture](docs/architecture/system-architecture.md) - the components
- [Wire protocol](docs/specs/wire-protocol.md) - the client/server format
- [Installing as a server](docs/operations/install.md)
- [Cutting a release](docs/operations/releasing.md)
- [Architecture decisions](docs/adr/) - every significant choice and its rationale

## Scope

PrismDB is **single-node** and **OLTP**-focused. Distribution/replication and
analytical (columnar) workloads are out of scope for v1. It does not speak the
Postgres, MongoDB, or Redis wire protocols - it has its own
[binary protocol](docs/specs/wire-protocol.md) and clients.

## License

Apache-2.0. See [LICENSE](LICENSE). Contributions: [CONTRIBUTING.md](CONTRIBUTING.md).
