//! The lock manager — per-RID write locks with deadlock detection.
//!
//! Under MVCC snapshot isolation, readers never lock; only writers do. A writer
//! takes an exclusive lock on a [`RecordId`] before modifying it and holds it
//! until the transaction commits or aborts (`release_all`). See
//! `docs/components/lock-manager.md`.
//!
//! Deadlock detection deviates from the doc's background-thread design: we detect
//! **synchronously at wait time**. When a transaction is about to block, we add
//! its wait-for edge and check whether that completes a cycle; if so, the
//! arriving transaction is the victim and gets [`CoreError::Deadlock`]. A cycle
//! is only ever completed by its final edge, so checking at each wait is
//! complete — and detection is immediate, with no background thread.

use std::collections::{HashMap, HashSet};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::TxnId;
use crate::error::{CoreError, Result};
use crate::record::RecordId;

/// Lock manager configuration.
#[derive(Clone, Copy, Debug)]
pub struct LockConfig {
    /// Number of lock-table shards (reduces contention across distinct RIDs).
    pub shards: usize,
    /// Default time a writer waits for a lock before giving up.
    pub default_timeout: Duration,
}

impl Default for LockConfig {
    fn default() -> Self {
        Self {
            shards: 64,
            default_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Default)]
struct LockState {
    holder: Option<TxnId>,
}

struct Shard {
    map: Mutex<HashMap<RecordId, LockState>>,
    cvar: Condvar,
}

/// Mediates concurrent write access to records.
pub struct LockManager {
    shards: Vec<Shard>,
    held: Mutex<HashMap<TxnId, Vec<RecordId>>>,
    graph: WaitForGraph,
    default_timeout: Duration,
}

impl LockManager {
    /// Create a lock manager with the given config.
    pub fn new(config: LockConfig) -> Self {
        let shards = (0..config.shards.max(1))
            .map(|_| Shard {
                map: Mutex::new(HashMap::new()),
                cvar: Condvar::new(),
            })
            .collect();
        Self {
            shards,
            held: Mutex::new(HashMap::new()),
            graph: WaitForGraph::default(),
            default_timeout: config.default_timeout,
        }
    }

    /// The configured default wait timeout.
    pub fn default_timeout(&self) -> Duration {
        self.default_timeout
    }

    /// Acquire an exclusive lock on `rid` for `txn`, blocking up to `timeout`.
    ///
    /// Re-entrant (a holder re-acquiring its own lock succeeds immediately).
    /// Returns [`CoreError::Deadlock`] if waiting would create a cycle, or
    /// [`CoreError::LockTimeout`] if the wait exceeds `timeout`.
    pub fn acquire(&self, txn: TxnId, rid: RecordId, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let shard = &self.shards[self.shard_index(rid)];
        let mut map = shard.map.lock().expect("lock shard poisoned");

        loop {
            let decision = {
                let state = map.entry(rid).or_default();
                match state.holder {
                    None => {
                        state.holder = Some(txn);
                        Decision::Acquired
                    }
                    Some(h) if h == txn => Decision::Reentrant,
                    Some(h) => Decision::Wait(h),
                }
            };

            match decision {
                Decision::Acquired => {
                    drop(map);
                    self.graph.clear(txn);
                    self.held
                        .lock()
                        .expect("held poisoned")
                        .entry(txn)
                        .or_default()
                        .push(rid);
                    return Ok(());
                }
                Decision::Reentrant => return Ok(()),
                Decision::Wait(holder) => {
                    if self.graph.set_wait_detect(txn, holder) {
                        self.graph.clear(txn);
                        return Err(CoreError::Deadlock);
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        self.graph.clear(txn);
                        return Err(CoreError::LockTimeout);
                    }
                    let (g, res) = shard
                        .cvar
                        .wait_timeout(map, deadline - now)
                        .expect("lock shard poisoned");
                    map = g;
                    if res.timed_out() {
                        self.graph.clear(txn);
                        return Err(CoreError::LockTimeout);
                    }
                    // Holder may have changed; loop and re-evaluate.
                }
            }
        }
    }

    /// Release every lock held by `txn`. Called by the transaction manager on
    /// commit or abort.
    pub fn release_all(&self, txn: TxnId) {
        let rids = self
            .held
            .lock()
            .expect("held poisoned")
            .remove(&txn)
            .unwrap_or_default();
        for rid in rids {
            let shard = &self.shards[self.shard_index(rid)];
            let mut map = shard.map.lock().expect("lock shard poisoned");
            if let Some(state) = map.get_mut(&rid) {
                if state.holder == Some(txn) {
                    state.holder = None;
                    shard.cvar.notify_all();
                }
            }
        }
        self.graph.clear(txn);
    }

    fn shard_index(&self, rid: RecordId) -> usize {
        let h = rid.page.as_u64().wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (rid.slot as u64);
        (h % self.shards.len() as u64) as usize
    }
}

enum Decision {
    Acquired,
    Reentrant,
    Wait(TxnId),
}

/// The wait-for graph: `txn -> the transactions it is waiting on`.
#[derive(Default)]
struct WaitForGraph {
    edges: Mutex<HashMap<TxnId, HashSet<TxnId>>>,
}

impl WaitForGraph {
    /// Record that `txn` now waits on `holder`, returning `true` if this creates
    /// a cycle (i.e. `holder` can already reach `txn`).
    fn set_wait_detect(&self, txn: TxnId, holder: TxnId) -> bool {
        let mut edges = self.edges.lock().expect("wait graph poisoned");
        edges.insert(txn, HashSet::from([holder]));
        reachable(&edges, holder, txn)
    }

    fn clear(&self, txn: TxnId) {
        self.edges.lock().expect("wait graph poisoned").remove(&txn);
    }
}

/// Depth-first reachability over the wait-for graph.
fn reachable(edges: &HashMap<TxnId, HashSet<TxnId>>, start: TxnId, target: TxnId) -> bool {
    let mut stack = vec![start];
    let mut seen = HashSet::new();
    while let Some(node) = stack.pop() {
        if node == target {
            return true;
        }
        if !seen.insert(node) {
            continue;
        }
        if let Some(next) = edges.get(&node) {
            stack.extend(next.iter().copied());
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_storage::PageId;
    use std::sync::Arc;
    use std::sync::Barrier;

    fn rid(p: u64, s: u16) -> RecordId {
        RecordId::new(PageId(p), s)
    }

    fn manager() -> LockManager {
        LockManager::new(LockConfig {
            shards: 16,
            default_timeout: Duration::from_secs(2),
        })
    }

    #[test]
    fn exclusive_then_timeout_then_release() {
        let lm = manager();
        let r = rid(1, 0);
        lm.acquire(10, r, Duration::from_millis(50)).unwrap();
        // Another txn can't acquire while 10 holds it.
        assert!(matches!(
            lm.acquire(11, r, Duration::from_millis(50)),
            Err(CoreError::LockTimeout)
        ));
        lm.release_all(10);
        // Now it's free.
        lm.acquire(11, r, Duration::from_millis(50)).unwrap();
        lm.release_all(11);
    }

    #[test]
    fn reentrant_acquire_succeeds() {
        let lm = manager();
        let r = rid(2, 0);
        lm.acquire(10, r, Duration::from_millis(50)).unwrap();
        lm.acquire(10, r, Duration::from_millis(50)).unwrap();
        lm.release_all(10);
    }

    #[test]
    fn waiter_proceeds_after_release() {
        let lm = Arc::new(manager());
        let r = rid(3, 0);
        lm.acquire(10, r, Duration::from_secs(5)).unwrap();

        let lm2 = lm.clone();
        let waiter = std::thread::spawn(move || {
            // Blocks until 10 releases, then succeeds.
            lm2.acquire(11, r, Duration::from_secs(5))
        });

        // Give the waiter time to block, then release.
        std::thread::sleep(Duration::from_millis(100));
        lm.release_all(10);
        waiter.join().unwrap().unwrap();
        lm.release_all(11);
    }

    #[test]
    fn deadlock_picks_one_victim() {
        let lm = Arc::new(manager());
        let (r1, r2) = (rid(4, 0), rid(5, 0));
        let barrier = Arc::new(Barrier::new(2));

        let lm1 = lm.clone();
        let b1 = barrier.clone();
        let t1 = std::thread::spawn(move || {
            lm1.acquire(100, r1, Duration::from_secs(5)).unwrap();
            b1.wait();
            let res = lm1.acquire(100, r2, Duration::from_secs(5));
            if res.is_err() {
                lm1.release_all(100); // victim releases so the other can proceed
            }
            res
        });

        let lm2 = lm.clone();
        let b2 = barrier.clone();
        let t2 = std::thread::spawn(move || {
            lm2.acquire(200, r2, Duration::from_secs(5)).unwrap();
            b2.wait();
            let res = lm2.acquire(200, r1, Duration::from_secs(5));
            if res.is_err() {
                lm2.release_all(200);
            }
            res
        });

        let r1res = t1.join().unwrap();
        let r2res = t2.join().unwrap();
        let deadlocks = [&r1res, &r2res]
            .iter()
            .filter(|r| matches!(r, Err(CoreError::Deadlock)))
            .count();
        assert_eq!(
            deadlocks, 1,
            "exactly one txn should be the deadlock victim: {r1res:?} {r2res:?}"
        );
        // The non-victim acquired its second lock.
        assert!(r1res.is_ok() ^ r2res.is_ok());
    }

    #[test]
    fn contention_all_proceed() {
        let lm = Arc::new(manager());
        let r = rid(6, 0);
        let threads: Vec<_> = (0..16u64)
            .map(|t| {
                let lm = lm.clone();
                std::thread::spawn(move || {
                    let txn = 1000 + t;
                    for _ in 0..10 {
                        lm.acquire(txn, r, Duration::from_secs(5)).unwrap();
                        lm.release_all(txn);
                    }
                })
            })
            .collect();
        for h in threads {
            h.join().unwrap();
        }
    }
}
