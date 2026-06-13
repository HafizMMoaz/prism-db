# Installing PrismDB as a server

`prismd` is a multi-database server, like MySQL or PostgreSQL: one service over a
single **data directory** that holds many named databases (each a subdirectory;
the reserved `_system` database holds the server-global user accounts). Clients
connect over TCP, authenticate once, then select a database with `USE`.

## Install the binaries

One installer per platform delivers `prismd`, `prism-shell`, `prism-fsck`, and
`prism-dump` together:

```sh
# Linux / macOS
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/HafizMMoaz/prism-db/releases/latest/download/prismdb-installer.sh | sh
# macOS / Linux via Homebrew
brew install HafizMMoaz/prism/prismdb
```

```powershell
# Windows
powershell -ExecutionPolicy Bypass -c "irm https://github.com/HafizMMoaz/prism-db/releases/latest/download/prismdb-installer.ps1 | iex"
```

Windows users can instead run the `.msi` from the
[latest release](https://github.com/HafizMMoaz/prism-db/releases/latest). To run
the server as a managed service that starts on boot, see
[Run as a service](#run-as-a-service) below.

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

Privileges are server-global by default but can be scoped to one database:

```sql
CREATE USER analyst WITH PASSWORD 'pw' ROLE none;  -- no access yet
GRANT readonly ON sales TO analyst;                -- read just `sales`
REVOKE ALL ON payroll FROM analyst;                -- deny `payroll` explicitly
SHOW GRANTS FOR analyst;                            -- global (*) + per-database
```

A per-database grant overrides the user's global role for that database (it can
widen *or*, as `REVOKE ALL ON <db>`, deny). User and grant management still
requires the `admin` role, as does `CREATE`/`DROP DATABASE`.

## Run as a service

### Linux — systemd

`prismd` reads its configuration from the environment, so the service is driven
entirely by an environment file (`/etc/prismdb/prismd.conf`): the bind address
(`PRISM_BIND`, **defaulting to localhost** — `admin`/`admin` is a development
credential), `PRISM_DATA_DIR`, `RUST_LOG`, and optional `PRISM_TLS_CERT` /
`PRISM_TLS_KEY`. The unit uses `DynamicUser=` + `StateDirectory=`, so it needs no
service account and owns `/var/lib/prismdb` itself.

```bash
# 1. Install the binary (the shell installer above puts it on your PATH;
#    a service wants it somewhere stable):
sudo install -m 0755 "$(command -v prismd)" /usr/bin/prismd

# 2. Install the config and the unit (both ship in deploy/).
sudo install -D -m 0644 deploy/prismd.conf    /etc/prismdb/prismd.conf
sudo install -D -m 0644 deploy/prismd.service /etc/systemd/system/prismd.service

# 3. Enable and start.
sudo systemctl daemon-reload
sudo systemctl enable --now prismd
systemctl status prismd
journalctl -u prismd -f          # structured logs, incl. the `audit` target
```

To accept remote connections, set `PRISM_BIND=0.0.0.0:4444` in
`/etc/prismdb/prismd.conf` (after configuring TLS and real accounts) and
`sudo systemctl restart prismd`.

### macOS — Homebrew service

Installed via Homebrew, run it under `brew services`:

```sh
brew services start prismdb      # starts now and on login
brew services stop  prismdb
```

### Windows — service

Wrap `prismd` with a service manager. [NSSM](https://nssm.cc) is simplest:

```powershell
nssm install PrismDB "C:\Program Files\PrismDB\prismd.exe" run
nssm set PrismDB AppEnvironmentExtra PRISM_DATA_DIR=C:\ProgramData\PrismDB\data PRISM_BIND=127.0.0.1:4444 RUST_LOG=info
nssm start PrismDB
```

Without NSSM, use Task Scheduler (“At startup”, run whether logged on or not)
with the same environment variables, or just run `prismd run` in a terminal for
development.

## Connecting

- **Shell:** `prism-shell <host>:<port> --user U --password P [--database D]`
- **Node.js:** `Client.connect({ host, port, username, password, database })`
- Any client: authenticate, then `USE <db>` (or pass the database at connect).

See `docs/specs/wire-protocol.md` for the protocol and `docs/operations/build-and-dev.md`
for building from source.
