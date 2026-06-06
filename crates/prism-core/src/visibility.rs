//! The snapshot-isolation visibility function.
//!
//! Decides whether a tuple version (identified by its [`RecordHeader`]'s `xmin`
//! and `xmax`) is visible to a reader's [`Snapshot`]. See `docs/components/mvcc.md`.
//!
//! Rules: a version is visible iff its creator's effect is visible to the
//! snapshot and its deleter's effect is not. A transaction `t`'s effect is
//! visible iff `t` is the reader itself, or `t` committed before the snapshot was
//! taken (i.e. `t < xmax`, `t` not in the active set, and `t`'s status is
//! `Committed`).

use crate::record::RecordHeader;
use crate::txn::{CommitLog, CommitStatus, Snapshot};
use crate::{NO_TXN, TxnId};

/// Whether `header`'s version is visible to `snapshot`.
pub fn visible(header: &RecordHeader, snapshot: &Snapshot, commits: &CommitLog) -> bool {
    if !effect_visible(header.xmin, snapshot, commits) {
        return false; // creation not visible to us
    }
    if header.xmax == NO_TXN {
        return true; // never deleted
    }
    if effect_visible(header.xmax, snapshot, commits) {
        return false; // deletion is visible to us
    }
    true // deleter is in-progress / aborted / after our snapshot
}

/// Whether transaction `t`'s effects are visible to `snapshot`.
fn effect_visible(t: TxnId, snapshot: &Snapshot, commits: &CommitLog) -> bool {
    if t == NO_TXN {
        return false;
    }
    if t == snapshot.txn_id {
        return true; // our own writes
    }
    if t >= snapshot.xmax {
        return false; // began after our snapshot
    }
    if snapshot.active.contains(&t) {
        return false; // in progress at our snapshot
    }
    matches!(commits.status(t), CommitStatus::Committed { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::RecordHeader;
    use crate::txn::{CommitLog, Snapshot};
    use prism_wal::Lsn;
    use proptest::prelude::*;
    use std::collections::HashSet;

    /// A commit log built directly for tests.
    fn commit_log(committed: &[TxnId], aborted: &[TxnId]) -> CommitLog {
        let log = CommitLog::new();
        for &t in committed {
            log.record_commit(t, Lsn::ZERO);
        }
        for &t in aborted {
            log.record_abort(t);
        }
        log
    }

    fn snap(txn_id: TxnId, xmax: TxnId, active: &[TxnId]) -> Snapshot {
        let active: HashSet<TxnId> = active.iter().copied().collect();
        let xmin = active.iter().copied().min().unwrap_or(txn_id).min(txn_id);
        Snapshot::new(txn_id, xmin, xmax, active)
    }

    fn hdr(xmin: TxnId, xmax: TxnId) -> RecordHeader {
        RecordHeader {
            xmin,
            xmax,
            prev_version: None,
            flags: 0,
        }
    }

    #[test]
    fn own_insert_is_visible() {
        let log = commit_log(&[], &[]);
        let s = snap(5, 5, &[]);
        assert!(visible(&hdr(5, NO_TXN), &s, &log));
    }

    #[test]
    fn own_delete_is_invisible() {
        let log = commit_log(&[], &[]);
        let s = snap(5, 5, &[]);
        assert!(!visible(&hdr(5, 5), &s, &log));
    }

    #[test]
    fn committed_before_snapshot_is_visible() {
        let log = commit_log(&[2], &[]);
        let s = snap(5, 5, &[]); // 2 < xmax, not active, committed
        assert!(visible(&hdr(2, NO_TXN), &s, &log));
    }

    #[test]
    fn began_after_snapshot_is_invisible() {
        let log = commit_log(&[7], &[]);
        let s = snap(5, 6, &[]); // xmax = 6, creator 7 >= xmax
        assert!(!visible(&hdr(7, NO_TXN), &s, &log));
    }

    #[test]
    fn concurrent_active_creator_is_invisible() {
        let log = commit_log(&[], &[]);
        let s = snap(5, 10, &[3]); // 3 active at snapshot time
        assert!(!visible(&hdr(3, NO_TXN), &s, &log));
    }

    #[test]
    fn aborted_creator_is_invisible() {
        let log = commit_log(&[], &[3]);
        let s = snap(5, 10, &[]);
        assert!(!visible(&hdr(3, NO_TXN), &s, &log));
    }

    #[test]
    fn in_progress_creator_is_invisible() {
        let log = commit_log(&[], &[]); // 3 has no status => InProgress
        let s = snap(5, 10, &[]);
        assert!(!visible(&hdr(3, NO_TXN), &s, &log));
    }

    #[test]
    fn deleted_by_committed_before_is_invisible() {
        let log = commit_log(&[2, 3], &[]);
        let s = snap(5, 5, &[]);
        assert!(!visible(&hdr(2, 3), &s, &log)); // created by 2, deleted by 3, both visible
    }

    #[test]
    fn deleted_by_in_progress_is_still_visible() {
        let log = commit_log(&[2], &[]); // 3 in progress
        let s = snap(5, 10, &[]);
        assert!(visible(&hdr(2, 3), &s, &log));
    }

    #[test]
    fn deleted_by_concurrent_active_is_still_visible() {
        let log = commit_log(&[2], &[]);
        let s = snap(5, 10, &[3]); // deleter 3 active at our snapshot
        assert!(visible(&hdr(2, 3), &s, &log));
    }

    #[test]
    fn bootstrap_txn_is_always_visible() {
        let log = CommitLog::new(); // seeds BOOTSTRAP_TXN as committed
        let s = snap(5, 5, &[]);
        assert!(visible(&hdr(crate::BOOTSTRAP_TXN, NO_TXN), &s, &log));
    }

    // ── Model-based property test ────────────────────────────────────────

    /// An independent (category-based) oracle for one transaction's visibility,
    /// structured differently from `effect_visible` to catch transcription bugs.
    fn oracle_effect_visible(
        t: TxnId,
        reader: TxnId,
        xmax: TxnId,
        active: &HashSet<TxnId>,
        committed: &HashSet<TxnId>,
    ) -> bool {
        if t == NO_TXN {
            false
        } else if t == reader {
            true
        } else if t >= xmax || active.contains(&t) {
            false
        } else {
            committed.contains(&t)
        }
    }

    proptest! {
        #[test]
        fn visibility_matches_oracle(
            xmin in 1u64..12,
            xmax_field in 0u64..12,
            reader in 2u64..12,
            horizon in 2u64..13,
            committed in proptest::collection::hash_set(1u64..12, 0..6),
            active in proptest::collection::hash_set(2u64..12, 0..6),
        ) {
            // A txn can't be both committed and active; active wins (in progress).
            let committed: HashSet<TxnId> =
                committed.difference(&active).copied().collect();
            let aborted: Vec<TxnId> = (1u64..12)
                .filter(|t| !committed.contains(t) && !active.contains(t) && *t != reader)
                .collect();

            let log = commit_log(&committed.iter().copied().collect::<Vec<_>>(), &aborted);
            let snapshot = Snapshot::new(
                reader,
                active.iter().copied().min().unwrap_or(reader).min(reader),
                horizon,
                active.clone(),
            );

            let got = visible(&hdr(xmin, xmax_field), &snapshot, &log);

            let xmin_vis = oracle_effect_visible(xmin, reader, horizon, &active, &committed);
            let expected = if !xmin_vis {
                false
            } else if xmax_field == NO_TXN {
                true
            } else {
                !oracle_effect_visible(xmax_field, reader, horizon, &active, &committed)
            };

            prop_assert_eq!(got, expected,
                "xmin={} xmax={} reader={} horizon={} active={:?} committed={:?}",
                xmin, xmax_field, reader, horizon, active, committed);
        }
    }
}
