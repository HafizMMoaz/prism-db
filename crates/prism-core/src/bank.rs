//! The bank-transfer harness - the M2 exit gate.
//!
//! A bank has N accounts whose balances sum to a constant. Each transfer moves
//! money between two accounts in one transaction, preserving the total. Two
//! tests exercise the whole transactional stack against the *total-balance
//! invariant*:
//!
//! 1. [`bank_transfer_survives_crash`] - single-threaded transfers plus one
//!    uncommitted "loser", then a simulated crash and `recover()`. After
//!    recovery the total must hold and the loser's transfer must be discarded
//!    (atomicity + durability + correct redo / loser handling).
//! 2. [`bank_transfer_concurrent_preserves_total`] - many threads transferring
//!    in parallel (each account guarded by a test-side mutex so transactions
//!    never spuriously conflict), verifying the total holds under concurrent
//!    commits and MVCC reads. DB-level write-lock contention is covered by the
//!    lock-manager tests.
//!
//! Accounts are addressed by their current `RecordId`, tracked test-side (the
//! test process never crashes - only the engine state is dropped), since
//! scan-after-recovery awaits the catalog.

use std::path::Path;
use std::sync::{Arc, Mutex};

use prism_buffer::{BufferPool, Config as BufConfig};
use prism_storage::DiskManager;
use prism_testkit::{Rng, TempDir};
use prism_wal::{Config as WalConfig, SyncMode, Wal};

use crate::record::RecordId;
use crate::recover;
use crate::store::{HeapId, RecordStore};
use crate::txn::{TxnManager, TxnMode};

const HEAP: HeapId = HeapId(7);

fn open_wal(dir: &Path) -> Arc<Wal> {
    Arc::new(
        Wal::open(
            &dir.join("wal"),
            WalConfig {
                segment_size: 256 * 1024,
                sync_mode: SyncMode::None,
            },
        )
        .unwrap(),
    )
}

fn read_balance(store: &RecordStore, txn: &crate::txn::TxnHandle, rid: RecordId) -> Option<i64> {
    store
        .read(txn, rid)
        .unwrap()
        .map(|v| i64::from_le_bytes(v[..8].try_into().unwrap()))
}

// ── Crash + recovery ────────────────────────────────────────────────────────

#[test]
fn bank_transfer_survives_crash() {
    for seed in 0..30u64 {
        run_bank_crash(seed);
    }
}

fn run_bank_crash(seed: u64) {
    let tmp = TempDir::new("bank-crash").unwrap();
    let heap = tmp.path().join("heap.db");
    let n = 6usize;
    let start = 1000i64;
    let total: i64 = n as i64 * start;
    let mut rng = Rng::new(seed ^ 0xBA17_C0DE);

    // Account -> current committed rid, and the test's model of balances.
    let mut rids: Vec<RecordId>;
    let mut model = vec![start; n];

    // ── Session 1: create accounts, run committed transfers, then a loser. ──
    {
        let disk = Arc::new(DiskManager::open(&heap, true).unwrap());
        let wal = open_wal(tmp.path());
        let buffer = Arc::new(
            BufferPool::new(disk.clone(), wal.clone(), BufConfig { frame_count: 8 }).unwrap(),
        );
        let txns = Arc::new(TxnManager::new(wal.clone()));
        let store = RecordStore::new(buffer.clone(), wal.clone(), txns.clone());

        let t = txns.begin(TxnMode::ReadWrite);
        rids = (0..n)
            .map(|_| store.insert(&t, HEAP, &start.to_le_bytes()).unwrap())
            .collect();
        t.commit().unwrap();

        let transfers = 5 + (seed % 12) as usize;
        for _ in 0..transfers {
            let a = rng.below(n as u64) as usize;
            let b = rng.below(n as u64) as usize;
            if a == b {
                continue;
            }
            let amt = 1 + rng.below(50) as i64;
            let t = txns.begin(TxnMode::ReadWrite);
            let ba = read_balance(&store, &t, rids[a]).unwrap();
            let bb = read_balance(&store, &t, rids[b]).unwrap();
            let new_a = store
                .update(&t, rids[a], &(ba - amt).to_le_bytes())
                .unwrap();
            let new_b = store
                .update(&t, rids[b], &(bb + amt).to_le_bytes())
                .unwrap();
            t.commit().unwrap();
            rids[a] = new_a;
            rids[b] = new_b;
            model[a] -= amt;
            model[b] += amt;
        }

        // One uncommitted transfer: writes versions + xmax stamps, never commits.
        let t = txns.begin(TxnMode::ReadWrite);
        let ba = read_balance(&store, &t, rids[0]).unwrap();
        let bb = read_balance(&store, &t, rids[1]).unwrap();
        let _ = store.update(&t, rids[0], &(ba - 77).to_le_bytes()).unwrap();
        let _ = store.update(&t, rids[1], &(bb + 77).to_le_bytes()).unwrap();
        std::mem::forget(t); // crash before commit/abort - a loser; model unchanged

        // Crash: drop everything without a clean flush.
        drop(store);
        drop(buffer);
        drop(txns);
        drop(disk);
    }

    // ── Recovery. ──
    let wal = open_wal(tmp.path());
    let report = {
        let disk = DiskManager::open(&heap, false).unwrap();
        let r = recover(&wal, &disk).unwrap();
        disk.close().unwrap();
        r
    };

    // ── Reopen and verify the invariant. ──
    let disk = Arc::new(DiskManager::open(&heap, false).unwrap());
    let buffer =
        Arc::new(BufferPool::new(disk.clone(), wal.clone(), BufConfig { frame_count: 8 }).unwrap());
    let txns = Arc::new(TxnManager::new_recovered(
        wal.clone(),
        report.next_txn_id,
        &report.committed,
        &report.aborted,
    ));
    let store = RecordStore::new(buffer, wal.clone(), txns.clone());

    let reader = txns.begin(TxnMode::ReadOnly);
    let mut sum = 0i64;
    for (i, &rid) in rids.iter().enumerate() {
        let bal = read_balance(&store, &reader, rid)
            .unwrap_or_else(|| panic!("seed {seed}: account {i} unreadable after recovery"));
        assert_eq!(bal, model[i], "seed {seed}: account {i} balance");
        sum += bal;
    }
    assert_eq!(
        sum, total,
        "seed {seed}: total-balance invariant after crash+recovery"
    );
    reader.commit().unwrap();
}

// ── Concurrency ─────────────────────────────────────────────────────────────

#[test]
fn bank_transfer_concurrent_preserves_total() {
    let tmp = TempDir::new("bank-conc").unwrap();
    let disk = Arc::new(DiskManager::open(&tmp.path().join("heap.db"), true).unwrap());
    let wal = open_wal(tmp.path());
    let buffer = Arc::new(
        BufferPool::new(disk.clone(), wal.clone(), BufConfig { frame_count: 32 }).unwrap(),
    );
    let txns = Arc::new(TxnManager::new(wal.clone()));
    let store = RecordStore::new(buffer, wal.clone(), txns.clone());

    let n = 4usize;
    let start = 100_000i64;
    let total: i64 = n as i64 * start;

    // Create accounts; track each account's current rid behind a mutex.
    let accounts: Vec<Mutex<RecordId>> = {
        let t = txns.begin(TxnMode::ReadWrite);
        let rids: Vec<_> = (0..n)
            .map(|_| store.insert(&t, HEAP, &start.to_le_bytes()).unwrap())
            .collect();
        t.commit().unwrap();
        rids.into_iter().map(Mutex::new).collect()
    };

    let threads = 4usize;
    let per_thread = 40usize;

    std::thread::scope(|s| {
        for tnum in 0..threads {
            let store = &store;
            let txns = &txns;
            let accounts = &accounts;
            s.spawn(move || {
                let mut rng = Rng::new(0xC0FFEE ^ tnum as u64);
                for _ in 0..per_thread {
                    // Pick two distinct accounts in index order (a < b) to keep
                    // both the rid-map mutexes and the record locks deadlock-free.
                    let mut a = rng.below(n as u64) as usize;
                    let mut b = rng.below(n as u64) as usize;
                    if a == b {
                        continue;
                    }
                    if a > b {
                        std::mem::swap(&mut a, &mut b);
                    }
                    let amt = 1 + rng.below(1000) as i64;
                    transfer(store, txns, accounts, a, b, amt);
                }
            });
        }
    });

    // The total must be exactly preserved.
    let reader = txns.begin(TxnMode::ReadOnly);
    let mut sum = 0i64;
    for acct in &accounts {
        let rid = *acct.lock().unwrap();
        sum += read_balance(&store, &reader, rid).expect("account visible");
    }
    assert_eq!(sum, total, "total-balance invariant under concurrency");
    reader.commit().unwrap();
}

/// Run one transfer between accounts `a < b`.
///
/// The account mutexes are held for the whole transfer (in `a < b` order, so
/// there is no deadlock), which serializes transfers that share an account at
/// the test level: each transaction therefore sees a stable, exclusively-held
/// pair of rows and never conflicts. Transfers on disjoint accounts still run
/// concurrently - exercising parallel commits, MVCC reads, and the invariant.
/// (DB-level write-lock contention is covered by the lock-manager tests.)
fn transfer(
    store: &RecordStore,
    txns: &TxnManager,
    accounts: &[Mutex<RecordId>],
    a: usize,
    b: usize,
    amt: i64,
) {
    let mut ga = accounts[a].lock().unwrap();
    let mut gb = accounts[b].lock().unwrap();
    let (rid_a, rid_b) = (*ga, *gb);

    let txn = txns.begin(TxnMode::ReadWrite);
    let ba = read_balance(store, &txn, rid_a).expect("account a visible");
    let bb = read_balance(store, &txn, rid_b).expect("account b visible");
    let new_a = store
        .update(&txn, rid_a, &(ba - amt).to_le_bytes())
        .unwrap();
    let new_b = store
        .update(&txn, rid_b, &(bb + amt).to_le_bytes())
        .unwrap();
    txn.commit().unwrap();

    // Publish the new rids only after the commit is durable.
    *ga = new_a;
    *gb = new_b;
}
