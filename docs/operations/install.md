# Installing PrismDB as a server

`prismd` is a multi-database server, like MySQL or PostgreSQL: one service over a
single **data directory** that holds many named databases (each a subdirectory;
the reserved `_system` database holds the server-global user accounts). Clients
connect over TCP, authenticate once, then select a database with `USE`.

## The data directory

`prismd` stores everything under one data directory — never in your project
folder. It is resolved in this order:

1. `--data <dir>` on the command line,
2. the legacy positional argument (`prismd run <dir>`),
3. the `PRISM_DATA_DIR` environment variable,
4. a platform default: `%ProgramData%\PrismDB\data` (Windows), `/var/lib/prismdb`
   (Linux, when it exists), else `~/.prismdb`.

```
$PRISM_DATA_DIR/
├── _system/      # server-global users (accounts, grants)
├── sales/        # a database
│   ├── heap.db
│   └── wal/
└── analytics/    # another database
```

The default account is `admin` / `admin` — change it before exposing the server.

## Quick start (any platform)

```bash
prismd init                       # initialize the default data directory
prismd run --bind 0.0.0.0:4444    # serve (durable: fsync on commit)
# from another machine/terminal:
prism-shell <host>:4444 --user admin --password admin --database sales
```

In a session: `SHOW DATABASES;`, `CREATE DATABASE sales;`, `USE sales;`, then
ordinary SQL / document / KV operations. `CREATE USER`, `GRANT`, `REVOKE` manage
accounts; TLS is enabled with `--tls-cert`/`--tls-key`.

## Linux — systemd service

```bash
# 1. Build and install the binary.
cargo build --release -p prism-server --bin prismd
sudo install -m 0755 target/release/prismd /usr/local/bin/prismd

# 2. Create a dedicated service account.
sudo useradd --system --no-create-home --shell /usr/sbin/nologin prismdb

# 3. Install and enable the unit (ships in deploy/prismd.service).
sudo install -m 0644 deploy/prismd.service /etc/systemd/system/prismd.service
sudo systemctl daemon-reload
sudo systemctl enable --now prismd

# 4. Check it.
systemctl status prismd
journalctl -u prismd -f          # structured logs, incl. the `audit` target
```

systemd creates and owns `/var/lib/prismdb` (via `StateDirectory=`), which the
unit passes as `PRISM_DATA_DIR`. Edit the unit to change the bind address, add
TLS, or tune `RUST_LOG`.

## Windows — service

`prismd` runs as a normal console process; wrap it with a service manager.
[NSSM](https://nssm.cc) is the simplest:

```powershell
# Install the binary somewhere stable, e.g. C:\Program Files\PrismDB\prismd.exe
nssm install PrismDB "C:\Program Files\PrismDB\prismd.exe" run --bind 0.0.0.0:4444
nssm set PrismDB AppEnvironmentExtra PRISM_DATA_DIR=C:\ProgramData\PrismDB\data RUST_LOG=info
nssm start PrismDB
```

Without NSSM, run it under Task Scheduler (“At startup”, run whether logged on or
not) with the same `PRISM_DATA_DIR` environment variable, or for development just
run `prismd run` in a terminal.

## Connecting

- **Shell:** `prism-shell <host>:<port> --user U --password P [--database D]`
- **Node.js:** `Client.connect({ host, port, username, password, database })`
- Any client: authenticate, then `USE <db>` (or pass the database at connect).

See `docs/specs/wire-protocol.md` for the protocol and `docs/operations/build-and-dev.md`
for building from source.
