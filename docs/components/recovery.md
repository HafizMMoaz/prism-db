# Component: Recovery

**Crate:** `prism-core`
**Status:** Accepted
**Last updated:** 2026-05-15

## Purpose

Recovery is the procedure run at startup to bring the database into a consistent state after a crash. It implements ARIES (Mohan et al. 1992) with three phases: analysis, redo, undo. It is the safety net behind the entire engine: as long as recovery is correct, the system survives any crash.

This is the document that, if wrong, makes everything else wrong.

## When recovery runs

- On every startup of the engine. Recovery is unconditional; if the previous shutdown was clean, the work is small (essentially zero pages to redo).
- After any panic that does not corrupt the WAL files.

Recovery is single-threaded in v1. The server does not accept connections until recovery completes.

## Inputs

- The heap file on disk (potentially with torn writes).
- The WAL segment files on disk (always assumed valid up to the last successful fsync; everything after is discarded).
- The database header on disk with the "last checkpoint LSN" field.

## Outputs

- The buffer pool, transaction manager, and commit log initialized to a consistent state.
- The active transaction table containing exactly the transactions that were committed at crash time (yes, even though they're not "active" anymore - see below) and aborted transactions removed.
- Wait - re-stating cleanly: after recovery, no transactions are active. Committed ones have their effects on pages; aborted/loser ones have been rolled back.

## Phase 1: Analysis

Scan the WAL forward from the last completed checkpoint, building:

1. The **dirty page table (DPT):** which pages were dirty at crash time. Bootstrapped from the checkpoint's snapshot; updated by every page-modifying record encountered.
2. The **active transaction table (ATT):** which transactions were active at crash time. Bootstrapped from the checkpoint's snapshot; updated by every Begin (implicit on first record per txn), Commit, and Abort encountered.

```
For each record R in [last_checkpoint, end_of_log]:
    if R.txn_id not in ATT and R is data-modifying:
        ATT.insert(R.txn_id, { state: Active, last_lsn: R.lsn, undo_next_lsn: R.lsn })
    
    match R.type:
        Insert | Update | Delete | Clr | IndexInsert | IndexDelete | PageSplit:
            DPT.insert_if_absent(R.page_id, R.lsn)   // rec_lsn
            ATT[R.txn_id].last_lsn = R.lsn
        Commit:
            ATT[R.txn_id].state = Committed
        Abort:
            ATT[R.txn_id].state = Aborted
        Checkpoint records: skip (already used as the starting point)
```

After analysis:
- DPT contains every page that may need redo.
- ATT contains every transaction's final state. Transactions still `Active` at end-of-log are **loser transactions** - they will be rolled back in undo.

The earliest `rec_lsn` in DPT is the start LSN for the redo phase.

## Phase 2: Redo

Scan the WAL forward from `min(rec_lsn over DPT)`, replaying every record whose effect may not be on disk.

For each record R:
1. If R is non-redoable (Commit, Abort, Checkpoint markers): skip.
2. Fetch page R.page_id into the buffer pool.
3. If page.page_lsn >= R.lsn: skip (the change is already on disk).
4. Otherwise:
   - Apply R's after-image (or the operation it encodes) to the page.
   - Set page.page_lsn = R.lsn.
   - Mark page dirty (it will be flushed eventually, but not now).

Idempotence: redo can be safely re-run. If the system crashes during redo, the next recovery's analysis will see the same WAL and produce the same DPT; the redo phase will skip records already applied (because page_lsn was advanced).

CLRs are also redone: applying a CLR re-applies its undo image. This is what makes the undo phase crash-safe: CLRs written by an interrupted undo are re-applied by the next recovery's redo, leaving the database in the partially-undone state, and undo resumes from there.

## Phase 3: Undo

For each loser transaction (still `Active` in ATT after analysis), walk backward through its WAL records and apply inverses.

The algorithm processes losers in parallel: at any moment, we know the "next LSN to undo" for each loser. We pick the loser whose next-LSN is largest (deepest in the log), undo that record, and update the loser's next-LSN to that record's `prev_lsn`. This way losers are undone in reverse total order - important for any inter-transaction dependencies.

For each step:
1. Identify the loser L with the largest undo_next_lsn.
2. If L.undo_next_lsn == 0: L is done, mark Aborted, remove from active losers.
3. Otherwise:
   - Fetch the record R at undo_next_lsn.
   - If R is a CLR: skip to R.undo_next_lsn (the original record's prev_lsn). CLRs are not themselves undone.
   - Otherwise:
     - Apply R's before-image to the page (the inverse operation).
     - Append a CLR: `Clr { txn: L, page: R.page, slot: R.slot, undo_image: R.before, undo_next_lsn: R.prev_lsn }`.
     - Set page.page_lsn = CLR's lsn; mark dirty.
   - Update L.undo_next_lsn = R.prev_lsn.

The CLR's `undo_next_lsn` field tells the next recovery where to resume undoing this transaction. If we crash mid-undo, the next recovery's analysis sees the CLR (the txn is still active in the ATT because we haven't written an Abort yet), and the undo phase resumes from the CLR's undo_next_lsn.

When a loser's undo_next_lsn reaches 0:
- Append `Abort { txn: L }`.
- Set state = Aborted.
- Remove from losers.

When all losers are done:
- `wal.flush_through(end_of_log)` - make sure the Abort records and CLRs are durable.
- Recovery completes.

## Bringing the active transaction table to live state

After undo, no transactions remain active. The TxnManager initializes:
- `next_txn_id = max_observed_txn_id + 1`
- `active_txns = empty`
- `commit_log` populated from the WAL's commit and abort records

The engine is now ready to accept new transactions.

## Heap repair

Pages may have torn writes from the crash. Two cases:

1. **Page was clean at crash time.** The on-disk page is intact (no in-flight write). Recovery doesn't touch it; it's already correct.
2. **Page was dirty at crash time.** It's in DPT. Recovery's redo replays every WAL record affecting it from `rec_lsn` forward, overwriting whatever torn bytes exist. After redo, page is correct.

The implicit assumption: pages that were not in DPT at crash time were either not dirty, or were dirty but flushed before crash (in which case they're correct on disk). The DPT bootstrap from the checkpoint is conservative - it can contain extra pages that were clean by crash time, but it cannot omit a page that was actually dirty (because every dirty-making operation goes through the WAL, and analysis re-derives DPT from those records).

## Full-page images

For paranoia about torn writes that span multiple records: on the first modification of a page after each checkpoint, the WAL record contains a full-page image instead of a delta. This is Postgres's full_page_writes feature; it doubles WAL volume in the worst case but guarantees that recovery can reconstruct any page regardless of how torn the on-disk image is.

v1 design: full-page images on first-modification-after-checkpoint, on by default. Configurable off for benchmarks (and clearly marked unsafe in that mode).

## Failure modes during recovery

| Failure | Behavior |
|---|---|
| WAL record CRC mismatch | Treat as end-of-log. Truncate WAL at that point. |
| Heap file unreadable | Fatal. Operator restores from backup. |
| Out of memory during DPT construction | Fatal. Operator increases memory or restores from a more recent backup. |
| Recovery itself crashes | Next restart re-runs recovery from scratch. Idempotent by design. |

## Performance

Recovery time is dominated by:
1. Reading the WAL from the checkpoint to end. Sequential I/O; throughput-bound.
2. Replaying records. CPU-bound on the page apply path.

A 100 MiB WAL with 1 GiB of dirty pages takes (empirically, on modern SSDs) under 30 seconds. Recovery time bound is our checkpoint frequency: more frequent checkpoints = shorter recovery = more disk I/O during normal operation. Default: every 5 minutes or every 64 MiB of WAL, whichever first.

## Configuration

```toml
[recovery]
checkpoint_interval_secs = 300
checkpoint_wal_threshold_mib = 64
full_page_images = true
parallel_redo = false           # v1: false; v2: true
```

## Metrics

- `prism_recovery_duration_seconds` (gauge, set at startup)
- `prism_recovery_records_replayed_total` (counter, reset each recovery)
- `prism_recovery_pages_redone_total`
- `prism_recovery_txns_undone_total`
- `prism_recovery_wal_truncated_bytes` (if end-of-log was a torn record)

## Testing

Recovery is the highest-priority test target.

- Unit: each of analysis, redo, undo on synthetic logs.
- Property: random workload generation, random `kill -9` during workload, restart, verify ACID properties hold on the resulting state.
- Fault injection: torn writes injected at the disk manager layer during workload; recovery must produce a consistent state.
- Soak: 24-hour run with random crashes every 30-90 seconds. No anomalies tolerated.

See `operations/fault-injection.md` for the full harness.

## References

- Mohan, Haderle, Lindsay, Pirahesh, Schwarz: "ARIES." ACM TODS 1992. The foundational paper. **Read this before touching this code.**
- ADR 0003 - WAL and ARIES choice.
- `components/wal.md` - log format and replay API.
- `components/transaction-manager.md` - commit log reconstruction.
- `operations/fault-injection.md` - the test harness.
