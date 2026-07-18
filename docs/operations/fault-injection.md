# Operations: Fault Injection

**Status:** Accepted
**Last updated:** 2026-05-15

A database earns trust by being crashed, killed, starved of disk, and starved of memory - repeatedly, under realistic load - and still coming back consistent. This document describes the fault-injection harness, what it tests, and the bar for accepting changes that touch recovery, the WAL, or the buffer pool.

## Goals

For any workload of committed transactions T1, T2, ..., Tn (where each transaction is durably committed before the next begins from a client's perspective):

1. **Atomicity:** After any number of crashes during the workload, either all of Ti's effects are visible or none are.
2. **Durability:** Every Ti whose commit returned successfully is visible after recovery.
3. **Consistency:** Every database invariant (no dangling index entries, slot counts match, version chains terminate, no orphan WAL records) holds after recovery.
4. **Isolation:** Snapshot isolation guarantees are preserved through crash and recovery.

The harness's job is to break the system in every way we can imagine, then verify these properties hold.

## Architecture

```
┌───────────────────────────────────────────────────────────────┐
│                     Harness controller                        │
│                                                               │
│   ┌──────────────┐  ┌──────────────┐  ┌──────────────────┐    │
│   │  Workload    │  │   Fault      │  │   Consistency    │    │
│   │  generator   │  │   injector   │  │   checker        │    │
│   └──────────────┘  └──────────────┘  └──────────────────┘    │
│         │                 │                  │                │
└─────────┼─────────────────┼──────────────────┼────────────────┘
          │                 │                  │
          ▼                 ▼                  ▼
    ┌─────────────────────────────────────────────────┐
    │       prismd (under test)                       │
    │                                                 │
    │   wrapped disk manager (injects I/O faults)     │
    │   wrapped allocator (injects allocation faults) │
    │   wrapped network (injects partitions, latency) │
    └─────────────────────────────────────────────────┘
          │
          ▼
       Test data dir on real filesystem
```

The harness runs in a sibling process (or container). The server is launched with a fault-injection-enabled build that wraps the disk manager, allocator, and network in instrumented shims.

## Fault categories

### Process faults

- **SIGKILL**: Immediate termination. The harshest case; the server has no opportunity to flush anything.
- **SIGTERM**: Graceful shutdown signal. The server should finish in-flight requests and exit clean.
- **SIGSTOP**: Pause the server (simulates a hypervisor pause). After a delay, SIGCONT and verify the server resumes correctly. Other clients see slow responses but no errors.

### Disk faults

The wrapped disk manager (`FaultyDiskManager`) can inject:

- **Torn writes**: A `write_page` call returns success but only the first K bytes were written (K randomly chosen, 0 < K < PAGE_SIZE).
- **Lost writes**: A `write_page` returns success but the bytes are not persisted.
- **Phantom reads**: A `read_page` returns bytes from a previous version of the page.
- **EIO**: Random I/O errors.
- **ENOSPC**: Out of disk space.
- **Slow I/O**: Inject latency (10 ms - 5 s) randomly.
- **fsync no-op**: `fdatasync` returns success without actually flushing.

Each fault is configured by:
- Probability per call (e.g., 0.001).
- Targeted page types (e.g., only WAL, only heap).
- Targeted phase (e.g., only during recovery).

### Memory faults

- **Allocation failure**: The allocator returns NULL on a random allocation.
- **OOM kill**: cgroups limit triggers, the kernel SIGKILLs the server.

### Network faults

- **Partition**: Drop all packets between two parties for a window.
- **Latency**: Inject 100-500 ms delay on each packet.
- **Reorder**: Deliver packets out of order.
- **Truncation**: A TCP connection closes mid-message.

Most network faults don't affect single-node correctness in v1 (no replication); they're tested for client-side robustness.

### Concurrency faults

- **Thread starvation**: Pin a worker thread to nothing.
- **Lock latency**: Inject delay in lock acquisition.

These help find latent bugs in lock ordering and timeout handling.

## Workloads

The harness drives one of several workloads:

### Random mixed
Random mix of SQL, document, and KV operations across multiple connections. Tests general resilience.

### Bank transfer
Classical example: many accounts, each transaction moves money between two accounts. Invariant: total balance is constant. Detects lost updates, partial commits.

### Counter race
Many transactions increment shared counters. Invariant: counter value equals the count of committed increments. Detects lost updates.

### Linearizable register
Single key, concurrent reads and writes. The history is checked with Elle for linearizability of single-key operations.

### Multi-model bank
Bank-transfer with audit records in a document collection and rate-limit counters in KV. Invariants:
- Sum of SQL balances is constant.
- Number of audit documents == number of committed transfers.
- Sum of rate-limit counters == number of committed transfers per (user, day).
- All three must hold *jointly*. This is the cross-model atomicity test.

## Consistency checker

After each crash and recovery cycle, the checker validates:

### Page-level
- Every page CRC matches.
- Every slot's `record_offset` is within the page, doesn't overlap others.
- Every slot's `record_length` doesn't exceed the page.
- `free_space_offset` and `free_space_end` are consistent with slots.

### Index-level
- Every B+tree leaf entry points at an existing record.
- B+tree keys are in order; leaves are linked.
- Hash entries match the bucket's hash prefix.

### Record-level
- Every record's `xmin` corresponds to a known transaction.
- `xmax = 0` or `xmax` is a known transaction.
- `prev_version` either is NIL or points at a valid record.
- Version chains terminate.

### Transaction-level
- Every committed transaction's effects are visible.
- Every aborted transaction's effects are absent.
- For the bank workload, balances sum to the expected total.

### Invariant-level (workload-specific)
- Bank: sum of balances == initial sum.
- Counter: counter value == count of committed increments.
- Multi-model: cross-model invariants hold.

If any check fails:
1. The harness captures: the database directory, the WAL, the workload journal, the harness's record of what was committed, recent server logs.
2. Halts.
3. Reports the failure with a reproduction guide.

## Run modes

### Fast mode (per-PR)
- 5 minutes.
- Moderate concurrency (8 connections).
- One workload (random mixed).
- One crash every 30-60 seconds.

Catches gross regressions. Run on every PR touching recovery, WAL, MVCC, buffer pool.

### Nightly mode
- 2 hours.
- High concurrency (50 connections).
- Cycle through all workloads.
- Random crashes every 5-30 seconds, plus 10% chance of torn write per page write.

### Weekly soak
- 24 hours.
- Mixed workload at sustained high load.
- Random crashes every 1-10 minutes.
- All fault types enabled.
- Memory profiling alongside to detect leaks.

## Reproducing failures

Every harness run uses a master seed; all randomness derives from it. A failure includes the seed; replaying the same seed reproduces the failure deterministically.

We commit failing seeds to `tests/regressions/` and replay them in CI to prevent regressions.

## Manual fault injection for development

Developers can run the harness against a local build:

```bash
prism-harness run \
  --workload bank \
  --connections 8 \
  --duration 5m \
  --crash-interval 30s \
  --fault-config "torn_writes=0.001,fsync_noop=0.0001" \
  --seed 0x1234 \
  --data-dir /tmp/harness
```

The harness restarts the server itself, runs the workload, kills/restarts at the configured interval, checks consistency.

## Failure triage

When the harness reports a failure:

1. Check the consistency report: which invariant broke?
2. Open the captured database with `prism-fsck` for a deeper audit.
3. Cross-reference the WAL with the workload journal: find the first divergence.
4. Reproduce locally with the seed; bisect crash points if needed.

`prism-fsck` is a separate offline tool (in the `prism-fsck` crate) that walks the database files and reports inconsistencies in human-readable form.

## Coverage of recovery code

The harness aims to crash at every observable state transition in the recovery code path:
- During WAL append.
- Between WAL append and fsync.
- Just before page write.
- Mid page write (torn).
- Just before commit fsync.
- Mid commit fsync.
- During checkpoint analysis.
- Mid checkpoint write.
- During redo phase.
- During undo phase.
- Mid CLR write.

The "kill at any point" workflow uses ptrace to pause the server at random instructions and SIGKILL from there, sampling the state space uniformly.

## Release criteria

For a v1 release:
- 30 consecutive nightly runs with no anomalies.
- One full weekly soak with no anomalies.
- Every workload's invariant verified across all crash modes.

## Limitations

The harness tests what we ask it to. Bugs in the harness, the consistency checker, or the model implementation are blind spots. We mitigate by:
- Code review of the harness with the same rigor as production code.
- A separate "harness self-test" that injects known bugs into the engine and verifies the harness catches them.

## References

- `components/recovery.md` - what we are testing.
- `operations/testing-strategy.md` - the broader test layering.
- Jepsen: https://jepsen.io
- Elle: https://github.com/jepsen-io/elle
- Mohan et al. 1992 - ARIES correctness arguments we are validating.
