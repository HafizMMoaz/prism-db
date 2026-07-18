//! A crash-simulation harness over the storage foundation.
//!
//! Drives a randomized buffer-pool write workload (each modification logged to
//! the WAL, like the record store will do), optionally injecting disk faults,
//! then simulates a crash by dropping all in-memory state and reopening the
//! files. It then checks the two guarantees the M1 foundation makes:
//!
//! 1. **No silent corruption.** A page that passes its checksum holds content
//!    we actually wrote; a torn page fails its checksum (it is *detected*, and
//!    Phase 2 recovery will redo it from the WAL).
//! 2. **The WAL invariant.** No heap page was ever written to disk ahead of the
//!    durable WAL (enforced and counted by [`FaultyDisk`]).
//!
//! With a faultless config and a clean shutdown, it additionally asserts full
//! durability: every page reads back its latest written value.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::OpenOptions;
use std::sync::Arc;

use prism_buffer::{BufferPool, Config as BufConfig, PageWriteGuard};
use prism_storage::{DiskManager, PAGE_SIZE, PageId, SlottedPageRef, StdFileBackend, checksum};
use prism_wal::record::RecordPayload;
use prism_wal::{Config as WalConfig, LogRecord, Lsn, SyncMode, Wal};

use crate::fault::{FaultConfig, FaultStats, FaultyDisk};
use crate::rng::Rng;
use crate::tempdir::TempDir;

/// Outcome of a crash-scenario run.
#[derive(Clone, Copy, Debug)]
pub struct CrashReport {
    /// Fault-injection counters from the run.
    pub stats: FaultStats,
    /// Pages inspected after reopen.
    pub pages_checked: usize,
    /// Pages whose checksum failed (torn/lost/uninitialized - detected).
    pub torn_detected: usize,
}

/// Run one crash scenario. Returns `Err` describing the first broken invariant,
/// or a [`CrashReport`] on success.
///
/// `steps` is the number of write operations; `clean_shutdown` flushes the pool
/// before the simulated crash (modeling an orderly exit).
pub fn run_scenario(
    seed: u64,
    cfg: FaultConfig,
    steps: usize,
    clean_shutdown: bool,
) -> Result<CrashReport, String> {
    let tmp = TempDir::new("crash").map_err(|e| e.to_string())?;
    let heap_path = tmp.path().join("heap.db");
    let wal_dir = tmp.path().join("wal");

    let wal = Arc::new(
        Wal::open(
            &wal_dir,
            WalConfig {
                segment_size: 256 * 1024,
                sync_mode: SyncMode::None,
            },
        )
        .map_err(|e| e.to_string())?,
    );

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&heap_path)
        .map_err(|e| e.to_string())?;
    let faulty = FaultyDisk::with_wal(Box::new(StdFileBackend::new(file)), cfg, seed, wal.clone());
    let handle = faulty.handle();
    let disk = Arc::new(DiskManager::with_backend(Box::new(faulty), 0));
    let pool = BufferPool::new(disk.clone(), wal.clone(), BufConfig { frame_count: 4 })
        .map_err(|e| e.to_string())?;

    let mut rng = Rng::new(seed ^ 0x5DEE_CE66_D000_1234);
    let mut ids: Vec<PageId> = Vec::new();
    let mut last: HashMap<PageId, u8> = HashMap::new();
    let mut seen: HashMap<PageId, HashSet<u8>> = HashMap::new();

    for _ in 0..steps {
        let v = (rng.next_u64() & 0xFF) as u8;
        let make_new = ids.is_empty() || rng.below(3) == 0;
        let pid = if make_new {
            let mut g = pool.new_page().map_err(|e| e.to_string())?;
            let pid = g.page_id();
            write_image(&mut g, v, &wal)?;
            ids.push(pid);
            pid
        } else {
            let pid = ids[rng.below(ids.len() as u64) as usize];
            let mut g = pool.fetch_write(pid).map_err(|e| e.to_string())?;
            write_image(&mut g, v, &wal)?;
            pid
        };
        last.insert(pid, v);
        seen.entry(pid).or_default().insert(v);

        if rng.below(5) == 0 {
            pool.flush_through(wal.last_lsn())
                .map_err(|e| e.to_string())?;
        }
    }

    if clean_shutdown {
        pool.flush_all().map_err(|e| e.to_string())?;
    }

    // ── Simulated crash: drop all in-memory state without flushing. ─────────
    drop(pool);
    drop(disk);
    drop(wal);

    // ── Reopen with a faithful disk and inspect what actually persisted. ────
    let disk2 = DiskManager::open(&heap_path, false).map_err(|e| e.to_string())?;
    let faultless_clean = clean_shutdown && cfg.is_faultless();
    let mut pages_checked = 0usize;
    let mut torn_detected = 0usize;

    for pidv in ids.iter().map(|p| p.as_u64()).collect::<BTreeSet<_>>() {
        let pid = PageId(pidv);
        let mut buf = [0u8; PAGE_SIZE];
        if disk2.read_page(pid, &mut buf).is_err() {
            continue; // page never reached the file; recovery would redo it
        }
        pages_checked += 1;

        if SlottedPageRef::new(&buf).verify_checksum() {
            let body = &buf[16..];
            let v0 = body[0];
            if !body.iter().all(|&b| b == v0) {
                return Err(format!(
                    "seed {seed:#x}: page {pidv} passed checksum but body is non-uniform (silent corruption)"
                ));
            }
            if !seen.get(&pid).is_some_and(|s| s.contains(&v0)) {
                return Err(format!(
                    "seed {seed:#x}: page {pidv} passed checksum with value {v0} that was never written"
                ));
            }
            if faultless_clean && last.get(&pid) != Some(&v0) {
                return Err(format!(
                    "seed {seed:#x}: clean faultless shutdown lost the latest write for page {pidv} \
                     (on-disk {v0}, expected {:?})",
                    last.get(&pid)
                ));
            }
        } else {
            torn_detected += 1;
            if faultless_clean {
                return Err(format!(
                    "seed {seed:#x}: page {pidv} failed checksum after a clean faultless shutdown"
                ));
            }
        }
    }

    let stats = handle.stats();
    if stats.violations > 0 {
        return Err(format!(
            "seed {seed:#x}: WAL invariant violated {} time(s) - a heap page was written ahead of the durable WAL",
            stats.violations
        ));
    }

    Ok(CrashReport {
        stats,
        pages_checked,
        torn_detected,
    })
}

/// Fill a pinned page with a uniform marker byte, log the change to the WAL,
/// stamp its `page_lsn`, and write a valid page checksum.
fn write_image(guard: &mut PageWriteGuard<'_>, value: u8, wal: &Wal) -> Result<(), String> {
    guard.fill(value);
    let page_id = guard.page_id();
    let lsn = wal
        .append(LogRecord::txn(
            1,
            Lsn::ZERO,
            RecordPayload::Insert {
                page_id,
                slot_id: 0,
                after_image: vec![value; 4],
            },
        ))
        .map_err(|e| e.to_string())?;
    guard.set_page_lsn(lsn);
    let bytes: &[u8; PAGE_SIZE] = guard;
    let crc = checksum::page_checksum(bytes);
    guard[8..10].copy_from_slice(&crc.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_faultless_shutdown_is_fully_durable() {
        for seed in 0..40u64 {
            let report = run_scenario(seed, FaultConfig::default(), 80, true)
                .unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(report.stats.torn, 0);
            assert_eq!(report.stats.lost, 0);
        }
    }

    #[test]
    fn torn_and_lost_writes_never_corrupt_silently() {
        let cfg = FaultConfig {
            torn_prob: 0.25,
            lost_prob: 0.10,
            ..FaultConfig::default()
        };
        let mut total_faults = 0u64;
        for seed in 0..160u64 {
            // run_scenario asserts: no silent corruption, WAL invariant holds.
            let report = run_scenario(seed, cfg, 80, false).unwrap_or_else(|e| panic!("{e}"));
            total_faults += report.stats.torn + report.stats.lost;
        }
        assert!(
            total_faults > 0,
            "fault injector never fired across 160 seeds"
        );
    }

    #[test]
    fn wal_invariant_holds_with_aggressive_eviction() {
        // Tiny pool + many pages => constant eviction; every heap write is
        // checked against the durable WAL inside FaultyDisk.
        for seed in 0..40u64 {
            let report = run_scenario(seed, FaultConfig::default(), 200, false)
                .unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(report.stats.violations, 0);
        }
    }
}
