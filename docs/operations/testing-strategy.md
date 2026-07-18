# Operations: Testing Strategy

**Status:** Accepted
**Last updated:** 2026-05-15

A database that loses or corrupts data is worse than no database. Tests are how we earn the right to call our system a database. This document describes what we test, how, and what bar each test category must clear.

## Test categories

We use a pyramid that is wider at the bottom than a typical web service:

```
                           Fault-injection
                          (24h soak; weekly)
                         ───────────────────
                        Jepsen-style anomaly
                       (nightly; full matrix)
                      ───────────────────────
                     Integration: full server
                    (per-commit, ~5 minutes)
                   ───────────────────────────
                  Property tests (proptest)
                 (per-commit, deterministic)
                ───────────────────────────────
               Unit tests
              (per-commit, fast)
             ───────────────────────────────────
```

Each layer catches different classes of bug. The unit tests catch obvious correctness bugs in isolation. The property tests catch interaction bugs the human-written cases miss. The integration tests catch wiring problems. The Jepsen tests catch concurrency anomalies. The fault-injection harness catches the bugs nothing else can find.

## Unit tests

In every crate, alongside the source. `cargo test -p <crate>` runs them.

What unit tests check:
- Pure functions and small modules.
- Every error case, not just the happy path.
- Boundary conditions: empty input, maximum input, off-by-one cases.

What unit tests do not check:
- Concurrency (use property tests with `loom` or stress tests).
- Persistence (use integration tests with an actual disk).

### Coverage expectation

We do not enforce a coverage percentage; coverage is reported but is not a gate. The expectation is qualitative: every public function has at least one test, every conditional branch in a critical module (recovery, MVCC, WAL) is covered.

## Property tests

We use `proptest` (https://github.com/proptest-rs/proptest). Every component with a non-trivial invariant has at least one property test.

Examples:

### MVCC visibility
```rust
proptest! {
    #[test]
    fn snapshot_isolation_visibility(txns in arb_txn_sequence(50)) {
        let oracle = ModelStore::new();
        let real = RealStore::new();
        for op in txns {
            let (m, r) = (oracle.apply(op.clone()), real.apply(op));
            prop_assert_eq!(m, r);
        }
    }
}
```

The "oracle" is a slow, obviously-correct reference implementation. The property test runs randomized sequences against both and verifies they agree.

### B+tree
```rust
proptest! {
    #[test]
    fn btree_invariants(ops in arb_btree_ops(1000)) {
        let mut tree = BTree::new();
        let mut model = BTreeMap::new();
        for op in ops {
            apply(&mut tree, &mut model, op);
            prop_assert!(tree.is_balanced());
            prop_assert!(tree.keys_in_order());
            prop_assert_eq!(tree.range_count(), model.len());
        }
    }
}
```

### WAL replay
```rust
proptest! {
    #[test]
    fn wal_replay_preserves_state(workload in arb_workload(1000)) {
        let wal = Wal::new(temp_dir());
        let pages_before = run_and_record_pages(&wal, &workload);
        let pages_after = replay_from_scratch(&wal);
        prop_assert_eq!(pages_before, pages_after);
    }
}
```

### Configuration

```toml
[profile.dev]
proptest_cases = 256       # default; CI runs 1024; nightly 10000
proptest_max_shrink_iters = 100000
```

When a property test fails, `proptest` automatically shrinks the input to a minimal failing case, which it persists in `proptest-regressions/`. Those regression files are committed; the next run replays them as deterministic tests.

## Integration tests

In `tests/` at the workspace root. Spin up a real server (in-process), connect a real client, exercise scenarios.

```rust
#[test]
fn cross_model_transaction_atomic() {
    let server = TestServer::start();
    let client = Client::connect(&server.endpoint()).await.unwrap();

    let result = client.tx.run(|tx| async move {
        tx.sql.execute("INSERT INTO users(id) VALUES(1)").await?;
        tx.kv.namespace("c").put("k", "v").await?;
        tx.documents.collection("d").insert_one(doc! { "x": 1 }).await?;
        Err::<(), _>(anyhow!("abort"))
    }).await;

    assert!(result.is_err());
    assert!(client.sql.query("SELECT * FROM users WHERE id=1").await.unwrap().is_empty());
    assert!(client.kv.namespace("c").get("k").await.unwrap().is_none());
}
```

### Categories

- **Smoke:** server starts, accepts a connection, runs a query, shuts down clean.
- **Cross-model:** transactions touching multiple models commit or roll back atomically.
- **Recovery:** kill the server mid-workload, restart, verify state.
- **Auth:** mTLS, password, failure cases.
- **Connection lifecycle:** drain, reconnect, idle timeout.

Integration tests run in CI on every commit. They take 3-5 minutes total.

## Crash and recovery testing

The most important tests. A dedicated harness:

```
loop:
  start server
  spawn workload generator
  after random delay in [5s, 60s]:
    kill -9 server
  wait for workload to detect disconnection
  start server again (recovery runs)
  run consistency check on the database:
    - every committed transaction's effects are present
    - no aborted transaction's effects are present
    - all indexes match the heap
    - all invariants hold
  if any check fails: capture state, halt, report
```

Crash-recovery testing runs nightly. The harness keeps a journal of the workload so failures are reproducible.

Detailed in `operations/fault-injection.md`.

## Jepsen-style anomaly testing

Elle (https://github.com/jepsen-io/elle) is the standard tool. It records a transaction history with timing and dependencies, then checks for anomalies post-hoc.

We test:
- Snapshot isolation: dirty reads, lost updates, unrepeatable reads, phantoms should be impossible. Write skew is allowed.
- Linearizability of single-key operations.

The harness runs concurrent workloads against a live server and feeds the resulting history to Elle. Anomalies are reported with the exact transaction sequence.

Runs nightly with default workload; weekly with the full test matrix (multiple workloads, varying concurrency).

## Fuzzing

`cargo-fuzz` targets for the most untrusted inputs:

- WAL record parser: random bytes through `LogRecord::decode`.
- Wire protocol parser: random bytes through the frame decoder.
- SQL parser: arbitrary strings through the binder.
- Document parser: arbitrary bytes through `Document::decode`.

```bash
cargo fuzz run wal_decode
cargo fuzz run wire_decode
cargo fuzz run sql_parse
cargo fuzz run doc_decode
```

Fuzz targets run continuously on a dedicated machine. Any crash gets a regression test added.

## Performance regression tests

`criterion`-based benchmarks (see `operations/benchmarking.md`) run nightly. CI compares against a 7-day rolling baseline and flags regressions > 10%.

## Long-running soak tests

24-hour soak runs weekly:
- High-concurrency mixed workload.
- Random crash injection every 1-10 minutes.
- Memory leak detection (RSS should reach steady state).
- Throughput stability (variance over the run).

The soak is the highest-confidence test; passing it for many consecutive weeks is the bar for the v1 release.

## Test data generation

`prism-bench` includes workload generators:

- TPC-C-like OLTP (relational): warehouses, customers, new-order/payment transactions.
- YCSB-like KV: configurable read/write ratio, hot-key skew.
- Synthetic document workload: realistic JSON documents (user profiles, events, logs).

Workloads are configurable for record count, concurrency, runtime, and seed for determinism.

## Test environment

- Local dev: `cargo test` runs against tmpfs by default; `PRISM_TEST_DATA_DIR=/path` to test on real disk.
- CI: ephemeral cloud VMs; logs and artifacts retained for 30 days.
- Soak: dedicated bare-metal machines (real SSDs, real disk semantics).

## Bug reproduction protocol

When a bug is reported:
1. Convert the symptom to a failing test (unit, integration, or harness scenario).
2. Add the test, verify it fails.
3. Fix the code, verify the test passes.
4. Run the relevant test category (property, harness) extra times to gain confidence.

The test, not the fix, is the deliverable.

## Test-only code

Test-only utilities live in `tests/common/` or in `#[cfg(test)]` modules within source files. They are not part of the public API and not subject to backward compatibility.

A `prism-testkit` crate (not published) holds reusable harnesses for integration tests across crates.

## Coverage of recovery

Recovery is the single highest-priority code path. Special practices:

- Every WAL record type has a test that writes, crashes, replays, verifies.
- The undo path has tests for every operation type, with `kill -9` injection at every step.
- Mid-undo crash followed by another mid-undo crash (recovery is idempotent across multiple crashes).
- Torn write injection at every page during workload.

If recovery is wrong, nothing else matters. We treat its test coverage accordingly.

## References

- `operations/build-and-dev.md` - how to run tests.
- `operations/fault-injection.md` - the crash-recovery harness.
- `operations/benchmarking.md` - performance regression suite.
- `components/recovery.md` - what we are testing the correctness of.
- proptest: https://github.com/proptest-rs/proptest
- Elle: https://github.com/jepsen-io/elle
