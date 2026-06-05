# Prism Design Documentation

This directory contains every design document for the Prism database engine. It is the contract for the implementation. If the code disagrees with the docs, one of them is wrong; the discrepancy gets resolved by amendment, not by drift.

## Structure

| Directory | Contents |
|---|---|
| [`overview/`](overview/) | Executive summary, vision, scope, glossary, roadmap |
| [`architecture/`](architecture/) | System-level architecture, data flow, module layout, threat model |
| [`adr/`](adr/) | Architecture Decision Records — every significant choice with rationale |
| [`components/`](components/) | Per-component design documents |
| [`specs/`](specs/) | Wire-level and on-disk specifications |
| [`operations/`](operations/) | Build, test, benchmark, observability, fault injection |
| [`project/`](project/) | Milestones, risk register, engineering standards |
| [`research/`](research/) | External references, prior art, papers |

## Reading order

**For a new engineer joining the project:**
overview → architecture → adr → components → specs

**For an external reviewer:**
overview/executive-summary.md → architecture/system-architecture.md → the four foundational ADRs (0001, 0003, 0004, 0006)

**For someone trying to implement a component:**
The component's design doc in `components/`, plus the specs it references, plus any ADRs it cites.

## Conventions

- Every document has a status: `Draft`, `Accepted`, `Superseded`, `Deprecated`.
- Every document has a last-updated date. Drift between code and docs is a defect.
- Diagrams are text-first (ASCII or Mermaid). Binary image formats only when text genuinely cannot express the diagram.
- Code samples are illustrative unless explicitly marked as normative.
