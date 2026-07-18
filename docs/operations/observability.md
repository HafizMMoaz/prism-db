# Operations: Observability

**Status:** Accepted
**Last updated:** 2026-05-15

Operators need to know what the server is doing. This document describes the three pillars - logs, metrics, traces - and the conventions for each.

## Logging

### Library

The `tracing` crate (https://tracing.rs) for structured logging. Every log line carries:
- Timestamp (RFC 3339, microsecond precision).
- Level (TRACE, DEBUG, INFO, WARN, ERROR).
- Target module path.
- Span context (request ID, transaction ID, connection ID where applicable).
- Message and structured fields.

### Format

Default output format is JSON, one event per line:

```json
{"ts":"2026-05-15T17:42:01.234567Z","level":"INFO","target":"prism_server::session","msg":"connection accepted","conn_id":"a1b2c3","peer":"10.0.0.5:54321"}
```

Configurable via `log_format = "json" | "pretty"`. `pretty` is for human consumption during development and produces non-JSON output. Production deployments use `json`.

### Levels

- `ERROR`: Something is wrong and operator attention may be needed. Examples: I/O failure, persistent retries, recovery anomaly, unauthorized access from a known IP.
- `WARN`: Something is off but the server is handling it. Examples: slow query, deadlock detected and resolved, connection limit approached.
- `INFO`: Significant events worth recording. Examples: server start/stop, recovery completed, checkpoint done, connection from new client, schema change.
- `DEBUG`: Detail for debugging specific scenarios. Off in production. Examples: per-query plan, per-page fetch.
- `TRACE`: Extreme detail. Off in production. Examples: every operator's `next()` call, every WAL append.

### Default

Production default: `RUST_LOG=info`. Operators can selectively enable debug or trace for specific modules: `RUST_LOG=info,prism_recovery=debug,prism_wal=trace`.

### Sensitive data

Logs do not include:
- Passwords, even hashed.
- Full SQL text (it may contain literals with PII). Statement digests are logged.
- Full document or row contents.

Logs do include:
- Object names (table names, collection names, user names).
- Operation types, affected counts, durations.
- Error codes and class.

### What to log

We err on the side of less. Verbose logging at INFO is a maintenance burden and slows shipping. Each new log line at INFO is reviewed for whether it adds operational value.

WARN and ERROR are different: they should fire only on things an operator cares about, and they should be actionable.

## Metrics

### Library

`metrics` crate with the Prometheus exporter. Metrics are exported via an HTTP endpoint (default `:9090/metrics`).

### Conventions

All metrics names are `prism_<subsystem>_<noun>_<unit>`:
- `prism_buffer_pool_pages_in_use`
- `prism_wal_flush_duration_seconds`
- `prism_txn_active`

Units: `_total` for cumulative counters, `_seconds` for durations, `_bytes` for sizes, `_count` for instantaneous counts.

Labels are used for dimensions:
- `prism_sql_queries_total{type="select"}`
- `prism_txn_aborted_total{reason="serialization_failure"}`

We avoid high-cardinality labels (no per-table, no per-user). Per-table metrics are exposed via the catalog at debug request, not via Prometheus.

### Catalog of essential metrics

#### Storage / WAL
- `prism_wal_appends_total`
- `prism_wal_flushes_total`
- `prism_wal_flush_duration_seconds` (histogram)
- `prism_wal_batched_commits` (histogram of commits-per-flush)
- `prism_wal_bytes_written_total`
- `prism_wal_segment_id_current`

#### Buffer pool
- `prism_buffer_hits_total`
- `prism_buffer_misses_total`
- `prism_buffer_evictions_total`
- `prism_buffer_dirty_pages` (gauge)
- `prism_buffer_pin_count` (gauge)

#### Transactions
- `prism_txn_active` (gauge)
- `prism_txn_started_total`
- `prism_txn_committed_total`
- `prism_txn_aborted_total{reason}`
- `prism_txn_lifetime_seconds` (histogram)

#### Locks
- `prism_lock_acquisitions_total`
- `prism_lock_wait_duration_seconds` (histogram)
- `prism_deadlocks_total`

#### Network
- `prism_net_connections_active` (gauge)
- `prism_net_connections_total`
- `prism_net_requests_total{type}`
- `prism_net_request_duration_seconds{type}` (histogram)

#### Engine
- `prism_sql_queries_total{type}`
- `prism_doc_operations_total{type}`
- `prism_kv_operations_total{type}`

### Histograms

Buckets are explicit, log-scale: 0.0001, 0.001, 0.01, 0.1, 1, 10 seconds (for durations); 64, 512, 4096, 32768, 262144, 2097152 bytes (for sizes).

### Endpoint

The metrics endpoint is exposed on a separate port (default 9090) from the main TCP listener. It has no authentication; operators are expected to firewall it appropriately. Future versions may add bearer-token auth.

## Tracing

### Library

OpenTelemetry via `tracing-opentelemetry`. The server emits spans for every request and significant internal operation.

### Span hierarchy

```
prism.session                          (long-lived; one per connection)
└── prism.request                      (one per request)
    └── prism.sql.execute              (or doc.X, kv.X)
        ├── prism.parse
        ├── prism.bind
        ├── prism.plan
        └── prism.execute
            ├── prism.operator.scan
            ├── prism.operator.filter
            └── prism.txn.commit
                └── prism.wal.flush
```

### Attributes

Standard OpenTelemetry conventions plus:
- `prism.txn_id`
- `prism.user`
- `prism.database`
- `prism.operation`
- `prism.affected_rows`
- `prism.serialization_failures`

### Sampling

Production sampling default: trace ratio 0.01 (1%) plus head-based sampling of errored requests (100%). Both configurable. The error trace bias makes debugging viable without retaining all traces.

### Export

OTLP/gRPC export to a configurable collector. The server does not bundle a tracing UI; deploy with Tempo, Jaeger, or whatever the operator already uses.

## Health endpoints

The metrics port also exposes:

- `GET /healthz` - returns 200 if the server is accepting connections; 503 during startup recovery or shutdown.
- `GET /readyz` - returns 200 if recovery has completed and the server is fully ready.
- `GET /metrics` - Prometheus exposition.

These are deliberate, narrow endpoints suitable for load balancer probes and orchestration. They do not require authentication.

## Slow query log

Queries exceeding a threshold (default 1 second) are logged at INFO with their statement digest, duration, and execution counters (rows examined, rows returned, pages fetched).

Configurable per-database via session variable `slow_query_threshold`.

## Crash diagnostics

On panic, the server:
1. Writes a panic report to `<data_dir>/diagnostics/panic-<timestamp>.log`. Contains:
   - Panic message and location.
   - Backtrace (with `RUST_BACKTRACE=1` automatic in release).
   - Snapshot of metrics.
   - Recent log buffer (last 1000 lines).
2. If the WAL is in a consistent state, exits cleanly (so recovery on restart is normal).
3. Otherwise, aborts (operators get a coredump if configured).

These reports are the artifact users send when filing bugs.

## Profiling endpoints

Built-in `pprof` endpoints on the metrics port:

- `GET /debug/pprof/profile?seconds=30` - CPU profile.
- `GET /debug/pprof/heap` - allocation snapshot (if `dhat` feature is built in).

Disabled by default in release builds; enabled with `--profiling-endpoint=on` flag. Reading the endpoint requires the `OPERATOR` role token.

## Configuration

```toml
[observability]
log_format = "json"
log_level = "info"
metrics_bind = "127.0.0.1:9090"
metrics_endpoint = "/metrics"
slow_query_threshold_ms = 1000

[observability.tracing]
enabled = false
otlp_endpoint = "http://otel-collector:4317"
sample_ratio = 0.01
```

## Operational practices

- **Treat warnings.** A persistent stream of WARN-level events indicates something to fix. Don't normalize warnings.
- **Track p99, not averages.** Average latency hides tail problems that hurt users.
- **Correlate metrics, logs, traces.** Every request log carries the trace ID; metrics are scoped by operation type that matches the trace span name.
- **Alert on saturation, not errors.** Errors happen; saturation (no headroom on connections, memory, or disk) is what causes outages.

## References

- `components/network-server.md` - metrics emitted there.
- `components/wal.md` - WAL-specific metrics.
- `operations/build-and-dev.md` - how to enable verbose logging in dev.
- `tracing` crate: https://tracing.rs
- `metrics` crate: https://docs.rs/metrics
- OpenTelemetry: https://opentelemetry.io
