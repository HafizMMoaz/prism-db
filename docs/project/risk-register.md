# Project: Risk Register

**Status:** Accepted (living document)
**Last updated:** 2026-05-15

Risks ranked by potential impact × likelihood. Each entry has an owner, a description of the failure mode, the mitigation strategy, and a status. Mitigations should not be aspirational - they should be specific things we will do.

## R1 - Recovery correctness

**Category:** Engineering
**Impact:** Catastrophic (data loss)
**Likelihood:** High (without effort)
**Owner:** TBD

**Description:** ARIES recovery is conceptually well-understood but has many edge cases. A bug in the analysis, redo, or undo phase can leave the database in an inconsistent state - silent data loss, wrong query results, or unable-to-start. The bug may not appear until production load hits an unusual crash pattern.

**Specific failure modes:**
- Off-by-one in the dirty page table reconstruction.
- Incorrect handling of CLRs during mid-undo crashes.
- Failure to update page_lsn after redo, causing re-redo on next recovery to be applied incorrectly.
- Active transaction table not bootstrapped correctly from checkpoint, leading to forgotten loser transactions.

**Mitigation:**
- All recovery code in `prism-core::recovery` is reviewed by at least two people.
- Property-based tests with the model-vs-real oracle pattern.
- Fault injection harness with 24-hour soaks; bank-transfer invariant.
- Code paths annotated with paper references (which sections of ARIES) for traceability.

**Status:** Open. All mitigations to be implemented during M2.

## R2 - Scope creep / time overrun

**Category:** Project
**Impact:** High (project doesn't ship)
**Likelihood:** High
**Owner:** TBD

**Description:** Database engines are notorious for absorbing scope. Each milestone has an "obvious" extra feature; we ship none of it well if we ship all of it.

**Mitigation:**
- ADRs already declare what is in and out of scope.
- Strict triage at every milestone: if it's not on the milestone list, it goes to post-v1.
- The roadmap commits to the build plan, including the honest probabilities.

**Status:** Continuous. Re-checked weekly.

## R3 - Performance is "fine" for benchmarks but bad in practice

**Category:** Engineering
**Impact:** Medium
**Likelihood:** Medium
**Owner:** TBD

**Description:** Our benchmark numbers may look reasonable while real workloads run into pathological cases: lock contention spikes, buffer pool thrashing on skewed access, version chains growing unbounded.

**Mitigation:**
- Adversarial benchmarks (`operations/benchmarking.md`) for skew, long chains, large records.
- Continuous metrics in production deployments, not just at release.
- We are honest in the documentation about what we have not tested.

**Status:** Open.

## R4 - fsync trust

**Category:** Engineering
**Impact:** Catastrophic (silent data loss)
**Likelihood:** Low but real
**Owner:** TBD

**Description:** Some filesystems and disk firmwares lie about fsync. A fsync returns success, but the data is not persistent. Power loss reveals the lie. This has affected every database at some point.

**Mitigation:**
- Use `fdatasync` on Linux (well-tested path).
- Use `F_FULLFSYNC` on macOS (the only reliable form there).
- Document the supported filesystems explicitly (ext4, xfs, apfs).
- Operator guidance to test with `diskchecker.pl` before production.
- On certain detected errors from fsync (EIO), the engine panics rather than continuing - the "fsync gate" pattern.

**Status:** Open. To be implemented in M1's WAL.

## R5 - Concurrency bugs in B+tree

**Category:** Engineering
**Impact:** High
**Likelihood:** Medium
**Owner:** TBD

**Description:** Lehman-Yao is well-defined, but concurrent index code is notoriously buggy. Lost updates, phantom keys, infinite loops on tree traversal.

**Mitigation:**
- Property tests with high case counts.
- Concurrent stress tests at varying thread counts.
- Code review with explicit attention to invariant preservation during splits.
- Reference the paper and PostgreSQL's nbtree for tricky cases.

**Status:** Open. To be implemented in M3.

## R6 - Cross-model transaction edge cases

**Category:** Engineering
**Impact:** High
**Likelihood:** Medium
**Owner:** TBD

**Description:** The differentiator of the project (cross-model ACID) is the easiest place to ship a subtle bug. A transaction touches SQL and KV; we crash between the SQL update and the KV update; recovery somehow restores partial state.

**Mitigation:**
- The single-WAL design (ADR 0006) makes this architecturally simple: every modification goes through the same WAL with the same TxnId.
- The multi-model bank-transfer workload in fault injection specifically targets this.
- Code review of every code path that produces WAL records to verify TxnId propagation.

**Status:** Open. Verified during M3.

## R7 - Out-of-scope SQL feature requested by users

**Category:** Project
**Impact:** Low (perception)
**Likelihood:** High
**Owner:** TBD

**Description:** First users will ask for things we don't have: full-text search, JSON functions in SQL, recursive CTEs, window functions. They are not in v1.

**Mitigation:**
- `vision-and-scope.md` is explicit about what is and isn't supported.
- The shell and SDK produce clear "unsupported" errors.
- We collect requests for prioritization in v1.1.

**Status:** Continuous.

## R8 - Memory leaks in long-running server

**Category:** Engineering
**Impact:** Medium (operations)
**Likelihood:** Medium
**Owner:** TBD

**Description:** Rust prevents most memory bugs, but doesn't prevent leaks: cycles in `Arc`, growing caches without bounds, lifetime mismatches that effectively retain data forever.

**Mitigation:**
- Soak tests measure RSS over 24 hours; growth beyond steady state is a regression.
- Bounded caches everywhere (commit log, lock manager, buffer pool).
- `dhat` profiling at release.

**Status:** Open.

## R9 - Documentation drift

**Category:** Project
**Impact:** Medium (developer experience)
**Likelihood:** High
**Owner:** TBD

**Description:** Docs and code diverge. The format spec says one thing; the implementation does another. Future maintainers (and current ones, after a few months) are misled.

**Mitigation:**
- ADRs are immutable once accepted; superseded ones are marked, not edited.
- Specs (`docs/specs/`) are referenced in code comments; changes require updating both.
- Documentation review is part of every milestone exit.

**Status:** Continuous.

## R10 - Single-developer bus factor

**Category:** Project
**Impact:** High
**Likelihood:** Medium
**Owner:** TBD

**Description:** Many decisions are in one head. If that person leaves or is unavailable, the project halts.

**Mitigation:**
- This documentation is the externalization of those decisions.
- ADRs capture rationale, not just decisions.
- Pair on the highest-stakes components (recovery, WAL).

**Status:** Continuous.

## R11 - napi-rs SDK packaging

**Category:** Engineering
**Impact:** Medium (adoption)
**Likelihood:** Low
**Owner:** TBD

**Description:** Node native modules can be a packaging headache: glibc version mismatches, missing prebuilts, opaque build errors.

**Mitigation:**
- Prebuilds for Linux x64/arm64, macOS arm64.
- Source build via `cargo` as fallback, with a clear error message if Rust isn't installed.
- Test installations in CI on the supported platforms.

**Status:** Open. M4.

## R12 - Tooling churn

**Category:** Engineering
**Impact:** Low
**Likelihood:** Medium
**Owner:** TBD

**Description:** Rust crate ecosystem moves fast. A dependency we pin today may be abandoned or break compatibility tomorrow.

**Mitigation:**
- Limited dependency set; prefer the standard library where reasonable.
- Lock files committed.
- Annual review of dependencies; cap at minor version unless we explicitly upgrade.

**Status:** Continuous.

## R13 - Hardware-specific bugs

**Category:** Engineering
**Impact:** High
**Likelihood:** Low
**Owner:** TBD

**Description:** Some bugs only appear on specific CPUs (memory ordering), specific filesystems (fsync semantics), or specific kernels (io_uring quirks).

**Mitigation:**
- CI runs on multiple Linux distributions and macOS.
- Documented supported environments (`operations/build-and-dev.md`).
- The community is encouraged to file detailed bug reports including environment.

**Status:** Continuous.

## Risk lifecycle

Each risk progresses through: Open → Mitigation Implemented → Verified → Closed.

A risk is **Closed** only after explicit verification (test passes, audit complete). Closing a risk based on "we did some work on it" is not allowed.

## Adding new risks

Anyone can add a risk. The template:
- Category, impact, likelihood, owner.
- Description of the failure mode.
- Specific failure modes (concrete scenarios, not abstractions).
- Mitigation strategy (what we will do, not what we hope).
- Status.

Risks discovered post-release are still added - they inform v1.1.

## References

- `project/milestones.md` - risks are mapped to milestones.
- `operations/testing-strategy.md` - how we verify mitigations.
- `operations/fault-injection.md` - the deepest mitigation effort.
