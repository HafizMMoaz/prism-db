# Contributing

Prism is in the design phase. Until v0.1 of the engine ships, contributions are documentation contributions.

## How design proposals work

1. **Open an issue first.** Describe what you want to change or add and why. If you are proposing a new component, sketch the public API.
2. **Write the doc.** New components require a design document under `docs/components/`. New significant decisions require an ADR under `docs/adr/`.
3. **Submit a PR.** Two reviewers required. Discussion happens in writing, in the PR, not in chat. Decisions made verbally are not decisions.
4. **Merge.** Once merged, the design is the contract. Implementations cite the doc; doc changes after implementation require a follow-up PR labeled `design-amendment`.

## ADR format

See `docs/adr/README.md`. Short version: every ADR has Context, Decision, Consequences, Status, and a date. ADRs are immutable once accepted; superseding an ADR means writing a new one that references the old one.

## What we will not accept

- Code without a corresponding design doc.
- Design docs that say "we will figure this out later."
- Performance numbers without a reproducible benchmark harness.
- Comparison claims against other systems without citations to their actual documentation.

## Communication

Design discussions: PR threads. Implementation discussions: PR threads. Quick clarifications: issues. Off-the-record speculation: not in this repository.
