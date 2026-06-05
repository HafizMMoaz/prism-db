# Project: Code Review Guide

**Status:** Accepted
**Last updated:** 2026-05-15

This document describes what reviewers look for and how reviews are conducted. Reviews exist to catch correctness bugs, design problems, and unclear code before they become permanent. They are a collaborative quality bar, not an adversarial gate.

## Review process

1. **Author opens a PR.** PR description states the goal, summarizes the approach, and lists anything risky.
2. **CI runs.** Failed CI must be addressed before review; reviewers don't read code that doesn't compile.
3. **One reviewer approves.** Two for changes to recovery, WAL, MVCC, or anything tagged `safety:critical`.
4. **Author addresses comments.** Either by changing the code or explaining why not. No "unaddressed" threads at merge time.
5. **Merge.** Squash or merge-commit per `engineering-standards.md`.

Reviews are not a hierarchy. Any contributor can review any PR. The expectation is that the reviewer engaged with the change, not just rubber-stamped.

## Design-doc-first rule

Significant changes (new components, new ADRs, new wire protocol messages) require a design doc or ADR before the PR. The PR references the design doc; the review is partly a verification that the implementation matches the design.

This rule exists because reviewing a 2000-line PR with no design context is a disaster. The doc-first workflow is slower in calendar days but much faster in total person-time.

What counts as "significant":
- A new public API.
- Changes to on-disk or wire format.
- New ADR-level decisions (new dependency, new algorithm, new abstraction).
- Cross-cutting refactors.

What doesn't:
- Bug fixes.
- Small refactors within a module.
- Documentation updates.
- Test additions.

## Review checklist

Reviewers work through this list. Not every item applies to every PR; skip what's irrelevant.

### Correctness

- [ ] Does the code do what the description and design doc say?
- [ ] Are there edge cases not handled? Empty inputs, max-size inputs, malformed inputs.
- [ ] Are errors handled (or explicitly ignored with a comment)?
- [ ] Are concurrency invariants preserved? Lock ordering, atomic memory ordering, drop guards.
- [ ] If the PR touches recovery: does it preserve the recovery invariants in `components/recovery.md`?
- [ ] If the PR touches MVCC: does it preserve snapshot isolation?
- [ ] If the PR touches the WAL: does it preserve the WAL invariant (no dirty page write before WAL is durable)?

### Tests

- [ ] Does the PR add tests for new behavior?
- [ ] Do the tests cover error paths, not just happy paths?
- [ ] If the change fixes a bug: is there a regression test for it?
- [ ] Are property tests appropriate for the change?
- [ ] Did CI run the relevant integration / harness tests?

### Performance

- [ ] Are there obvious performance regressions? (Don't assume; if uncertain, ask for a benchmark.)
- [ ] Are there allocations or syscalls in tight loops that weren't there before?
- [ ] Is the change in the hot path (per-tuple, per-page)? If yes, extra scrutiny.

### Style and clarity

- [ ] Names communicate purpose.
- [ ] Comments explain why, not what.
- [ ] Public items have doc comments.
- [ ] Module organization makes sense.
- [ ] Code is readable on its own without the reviewer's full context.

### Safety

- [ ] Every `unsafe` block has a `// SAFETY:` comment.
- [ ] No new dependencies without justification.
- [ ] No new system calls or filesystem assumptions without consideration of their portability.
- [ ] No new global state.

### Documentation

- [ ] Public API has doc comments.
- [ ] Behavior changes are reflected in the relevant `docs/`.
- [ ] Specifications updated if formats change.
- [ ] Glossary updated if new terminology introduced.

## Critical-path components

Higher review bar applies to:

- `prism-wal` — durability is everything.
- `prism-core::recovery` — correctness defines the system.
- `prism-core::mvcc` — the visibility function is one of two cornerstones (the other is recovery).
- `prism-core::txn_manager` — transactional state machine.
- `prism-buffer` — WAL invariant lives here.

For PRs touching any of these:
- Two reviewers required.
- The fault-injection harness must be run on the branch before merge (an automated CI job).
- Reviewers explicitly verify the relevant invariants are preserved.

## Comment style

Be specific. "This is wrong" is not a comment; "this misses the case where `xmax == txn_id` and the txn is committed" is.

Distinguish:
- **Blocking:** must change before merge. Use clearly: "blocking: ..." or "must fix: ...".
- **Strong suggestion:** the reviewer thinks this should change but won't block. "Suggest: rename `next_version` to `prev_version` for clarity."
- **Nit:** small style or wording preference. "Nit: trailing whitespace."
- **Question:** the reviewer doesn't understand. "Why does this hold the lock past the await?"
- **Praise:** call out things done well. Surprisingly important for morale.

Reviewers use the prefixes; authors know what's required vs. optional.

## When to push back

Authors should push back when:
- The reviewer is wrong on the facts.
- The suggestion would degrade the design.
- The cost of the change outweighs the benefit.

The exchange should focus on the technical question. "I disagree because X" is fine; "I disagree because reasons" is not. If two senior contributors disagree, escalate to a third for tiebreaker.

## When the author is the only one who can review

For solo or small-team development, the "two reviewers" rule isn't always possible. Workarounds:

- Self-review: walk through the diff as a reviewer would, applying the checklist. Document findings.
- Time-shift: write the change today, review your own PR tomorrow with fresh eyes.
- Lower the bar to one reviewer for non-critical changes; keep two for critical-path changes by waiting until a second reviewer is available.

What we do not do: skip review entirely. Reviewing your own work is real; merging without any review is not.

## Approving with reservations

A reviewer may approve a PR while noting unresolved concerns:

> "Approved. Two non-blocking concerns: (a) the locking pattern in `acquire_x` is fragile but works; consider refactoring in a follow-up. (b) The test coverage for the deadlock path is light."

The author is expected to file follow-up issues for these. The reservations are part of the PR record.

## Velocity vs. quality

Both matter. Slow reviews kill momentum; rushed reviews ship bugs.

Soft norms:
- Reviewers respond within one business day, even if just to say "I'll get to this tomorrow."
- Authors don't add commits to a PR after it's been approved (except to address review comments); large changes get a new PR.
- The author may merge without waiting for the reviewer to re-approve trivial fixups, marking them clearly: "fixup: address review."

## Special review types

### Doc-only PRs

Reviewers check: technical accuracy, clarity, consistency with other docs, no broken links. One reviewer.

### Test-only PRs

Reviewers check: does the test test what it claims? Is it deterministic? Does it fail when the bug it targets is present? One reviewer.

### Dependency updates

Reviewers check: changelog of the dependency, security advisories, transitive dependency changes, CI passes. One reviewer for patches; two for major versions.

### Refactors

Reviewers check: behavior unchanged (tests prove this), the new structure is genuinely better, the diff isn't bigger than it needs to be. One reviewer for small refactors; two for cross-cutting ones.

## Disagreements that don't resolve

When author and reviewer can't agree on a substantive point:

1. State the disagreement clearly in the PR.
2. Bring in a third opinion.
3. If still unresolved: defer the decision to a design doc / ADR. Don't ship the change until the question is settled.

Burning a week on a PR over a stuck disagreement is worse than the cost of writing an ADR.

## What we don't do

- We don't review for "matches my personal style."
- We don't gatekeep on irrelevant grounds ("I would have written this differently" without a reason).
- We don't approve to be polite. A reviewer who can't engage with the code should say so.

## References

- `project/engineering-standards.md` — what we expect code to look like.
- `operations/testing-strategy.md` — what tests we expect to see.
- `project/risk-register.md` — what we're afraid of.
