# Security Policy

PrismDB is a database engine: a vulnerability here can mean data loss, data
disclosure, or remote compromise. We take reports seriously and ask that you
report privately so a fix can ship before the issue is public.

## Supported versions

PrismDB is pre-1.0. Security fixes are applied to the `main` branch and the most
recent tagged release only.

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |
| < 0.1   | :x:                |

## Reporting a vulnerability

**Do not open a public issue for a security vulnerability.**

Use GitHub's private vulnerability reporting instead:

1. Go to the **Security** tab of the repository.
2. Click **Report a vulnerability**.
3. Fill in the advisory form with as much detail as you can.

This opens a private channel visible only to you and the maintainers. If you
cannot use GitHub's reporting flow, open a normal issue that says only "I would
like to report a security issue, please enable private reporting" — with no
technical detail — and we will follow up.

### What to include

- The affected component (engine, WAL/recovery, buffer pool, SQL/document/KV
  access methods, network protocol, authentication/authorization, or an SDK).
- The version, commit SHA, and platform (Linux / macOS / Windows).
- A minimal reproduction: SQL, a wire-protocol sequence, or a short program.
- The impact you believe it has (data loss, disclosure, privilege escalation,
  denial of service, etc.).

### What to expect

- **Acknowledgement** within 5 business days.
- **An initial assessment** (severity, whether we can reproduce) within 10
  business days.
- **Coordinated disclosure.** We will agree a disclosure date with you, fix the
  issue on a private branch, publish a release and a GitHub Security Advisory,
  and credit you unless you prefer to remain anonymous.

## Scope

In scope: anything that compromises confidentiality, integrity, or availability
of data managed by PrismDB, or that allows code execution or privilege
escalation against the server or its clients.

Out of scope: vulnerabilities in third-party dependencies (report those
upstream; tell us if PrismDB's use of them is exploitable), issues that require
an already-compromised host or physical access, and theoretical weaknesses with
no demonstrated impact.

## Hardening notes

PrismDB ships durable-by-default and authenticated-by-default:

- Passwords are hashed with scrypt; they are never logged (the audit log
  redacts them).
- The network listener supports TLS (rustls / ring backend).
- Authorization is enforced per request via a READ/WRITE/ADMIN privilege model.

See [`docs/operations/install.md`](docs/operations/install.md) for the
recommended production deployment (dedicated service account, restricted data
directory, the hardened systemd unit in [`deploy/prismd.service`](deploy/prismd.service)).
