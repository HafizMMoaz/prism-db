# Threat Model

**Status:** Accepted
**Last updated:** 2026-05-15

This document enumerates the threats Prism is designed to defend against and the threats it explicitly does not. A v1.0 self-managed single-node database has a narrower threat surface than a cloud-managed multi-tenant service, but it is not zero.

## Trust boundaries

```
┌─────────────────────────────────────────────────────────────┐
│                       Untrusted                              │
│                                                              │
│  Network clients (SDK users, shell users, ad-hoc TCP)        │
│                                                              │
└──────────────────────────┬───────────────────────────────────┘
                           │  TLS (optional but recommended)
                           │  Authentication required
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                       Trusted                                │
│                                                              │
│  Prism server process                                        │
│  ├── Configuration file                                      │
│  ├── Heap file (data)                                        │
│  ├── WAL files                                               │
│  └── Backup files                                            │
│                                                              │
│  OS user account running the server                          │
│  Filesystem permissions on data directory                    │
│                                                              │
└──────────────────────────────────────────────────────────────┘
```

Everything inside the trusted boundary is assumed to be controlled by the operator. The threat model concerns what crosses the boundary.

## Threats in scope

### T1. Unauthenticated network access
An attacker on the network attempts to query or modify the database without credentials.

**Mitigations:**
- TCP server requires authentication before any query is accepted.
- Default configuration binds to localhost; binding to a public interface requires explicit configuration.
- TLS is supported and recommended for any non-localhost deployment.

### T2. Credential theft via passive network observation
An attacker captures network traffic and learns user passwords or session tokens.

**Mitigations:**
- TLS for transport encryption (operator-configured).
- Password authentication uses challenge-response; raw passwords are never transmitted.
- Credentials in the catalog are scrypt-hashed.

### T3. SQL / document / KV injection
A client embeds untrusted data into a query string in a way that allows escaping into the query syntax.

**Mitigations:**
- The SDK supports parameterized queries as the primary API; string concatenation is documented as discouraged.
- Document and KV APIs do not accept query strings at all; predicates and keys are structured.
- The shell warns when statements look hand-constructed from user input (heuristic).

### T4. Authorization bypass
An authenticated user attempts to read or modify data they should not have access to.

**Mitigations:**
- Role-based access control on tables, collections, and namespaces.
- Operator role required for catalog modifications.
- Every operation checks ACL before execution.
- Failed authorization is logged with the requesting user.

### T5. Denial of service via resource exhaustion
A client opens many connections, submits expensive queries, or fills the WAL faster than it can be archived.

**Mitigations:**
- Per-connection rate limits.
- Query execution timeout (configurable, default 30s).
- Max in-flight transactions per user.
- Connection limit per user and global.
- WAL retention policy with backpressure when archive is too far behind.

### T6. Data corruption from buggy I/O or storage layer
A bit-flip in flight or on disk corrupts a page.

**Mitigations:**
- Every page carries a CRC32 checksum, validated on read.
- WAL records carry CRCs.
- `fsync` semantics are explicit; we do not rely on filesystem journaling.
- `prism-fsck` for offline integrity checks.

### T7. Recovery correctness under crash
Crashes or power loss mid-write leave the database in an inconsistent state.

**Mitigations:**
- ARIES recovery with full redo and undo.
- WAL durability invariant (log before page).
- Idempotent recovery (crashes during recovery are safe).
- Fault injection in CI.

This is not really a "threat" in the security sense, but the engineering machinery is identical.

## Threats out of scope (v1)

### T8. Side-channel attacks
Timing attacks against authentication, cache-based attacks against MVCC visibility, etc. We do not constant-time-compare passwords beyond what `scrypt`'s verify function provides. A determined attacker with co-located code can probably extract information; we accept this for v1.

### T9. Insider threats
An operator with shell access to the server can read the heap file directly. Disk encryption is the operator's responsibility (LUKS, filesystem-level encryption); Prism does not encrypt at rest in v1.

### T10. Supply chain
We do not vendor dependencies or audit every transitive crate. We pin versions in `Cargo.lock`, run `cargo audit` in CI, and accept the residual risk.

### T11. Hardware-level attacks
Cold boot attacks, DMA attacks, malicious firmware. Out of scope.

### T12. Multi-tenancy isolation
v1 assumes one customer per Prism instance. Hard isolation between databases on the same process is not a goal. Multi-tenant deployments should use process-per-tenant.

### T13. Privilege escalation against the OS
We assume the server runs as a non-root user with minimum filesystem permissions on the data directory. Privilege escalation outside the Prism process (e.g., exploiting the OS) is the operator's problem.

## Data classification

What lives where:

| Asset | Where | Sensitivity |
|---|---|---|
| User credentials (hashed) | Catalog, on disk | High — encrypt the filesystem |
| Session tokens | In-memory only | High — never written to disk |
| User data | Heap file | Operator's classification, treat as max |
| WAL | WAL files | Same as user data |
| Configuration | TOML file | Medium — may contain TLS keys |
| TLS private keys | Operator-managed path | Critical |
| Logs | stdout or file | Medium — may contain query text |

## Logging and audit

- Every authentication attempt logged with timestamp, user, source IP, success/failure.
- Every authorization failure logged.
- Every operator-level catalog change logged.
- Query text is **not** logged by default (PII risk); enable `log_queries = true` to opt in.

Logs are structured JSON, suitable for ingestion into a SIEM.

## Open security questions

1. **Should we support row-level security?** Postgres-style policies attached to tables. Likely yes, but not in v1. Tracked as a post-v1 feature.
2. **Should we support encryption at rest in the engine?** Cleaner than relying on filesystem encryption, but adds key management complexity. Likely no for v1.
3. **Should the shell store credentials?** A shell with a `~/.prismrc` is convenient but creates a credential file. v1: no, prompt every time or use environment variables.

## Threat review cadence

This document is reviewed:
- Before each minor release.
- When a new feature crosses a trust boundary.
- After any security incident.
