//! The [`BufferPool`]: frames, RAII page guards, clock-sweep eviction, and the
//! WAL invariant.
//!
//! Locking discipline (to avoid deadlock): the directory mutex is always
//! acquired **before** any per-frame state mutex. The directory mutex doubles
//! as the "allocation latch" — it is held across the whole cache-miss/eviction
//! path so only one thread evicts at a time. Per-frame content latches
//! (`RwLock`) are acquired *after* releasing the directory mutex (the frame is
//! pinned first, so it cannot be evicted while we wait).
//!
//! This uses `std` `Mutex`/`RwLock` and a `HashMap` page table. Sharding the
//! directory (e.g. `DashMap`) to reduce contention is a documented follow-up;
//! it does not change correctness.

use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use prism_storage::{DiskManager, PAGE_SIZE, PageId};
use prism_wal::{Lsn, Wal};

use crate::error::{BufferError, Result};

/// Buffer pool configuration.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Number of page frames in the pool.
    pub frame_count: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self { frame_count: 1024 }
    }
}

impl Config {
    /// A config sized to roughly `mib` mebibytes of page cache.
    pub fn from_pool_mib(mib: usize) -> Self {
        Self {
            frame_count: (mib * 1024 * 1024 / PAGE_SIZE).max(1),
        }
    }
}

struct FrameState {
    page_id: Option<PageId>,
    pin_count: u32,
    usage_count: u8,
    dirty: bool,
    page_lsn: Lsn,
}

struct Frame {
    content: RwLock<Box<[u8; PAGE_SIZE]>>,
    state: Mutex<FrameState>,
}

impl Frame {
    fn empty() -> Self {
        Self {
            content: RwLock::new(Box::new([0u8; PAGE_SIZE])),
            state: Mutex::new(FrameState {
                page_id: None,
                pin_count: 0,
                usage_count: 0,
                dirty: false,
                page_lsn: Lsn::ZERO,
            }),
        }
    }
}

/// Directory state guarded by a single mutex (the allocation latch).
struct Directory {
    page_table: HashMap<PageId, usize>,
    dirty: HashSet<PageId>,
    clock_hand: usize,
}

/// The in-memory page cache.
pub struct BufferPool {
    frames: Vec<Frame>,
    dir: Mutex<Directory>,
    disk: Arc<DiskManager>,
    wal: Arc<Wal>,
}

const MAX_USAGE: u8 = 3;

impl BufferPool {
    /// Create a buffer pool over `disk` and `wal` with the given config.
    pub fn new(disk: Arc<DiskManager>, wal: Arc<Wal>, config: Config) -> Result<Self> {
        let n = config.frame_count.max(1);
        let frames = (0..n).map(|_| Frame::empty()).collect();
        Ok(Self {
            frames,
            dir: Mutex::new(Directory {
                page_table: HashMap::new(),
                dirty: HashSet::new(),
                clock_hand: 0,
            }),
            disk,
            wal,
        })
    }

    /// Pin a page for reading, loading it from disk on a miss.
    pub fn fetch_read(&self, page_id: PageId) -> Result<PageReadGuard<'_>> {
        let idx = self.pin_for_access(page_id)?;
        let frame = &self.frames[idx];
        let content = frame.content.read().expect("content latch poisoned");
        Ok(PageReadGuard { frame, content })
    }

    /// Pin a page for writing, loading it from disk on a miss.
    pub fn fetch_write(&self, page_id: PageId) -> Result<PageWriteGuard<'_>> {
        let idx = self.pin_for_access(page_id)?;
        let frame = &self.frames[idx];
        let content = frame.content.write().expect("content latch poisoned");
        Ok(PageWriteGuard {
            frame,
            pool: self,
            content: Some(content),
            page_id,
        })
    }

    /// Allocate a fresh page on disk and pin it for writing (zero-filled).
    pub fn new_page(&self) -> Result<PageWriteGuard<'_>> {
        let page_id = self.disk.allocate_page()?;
        let idx = {
            let mut dir = self.dir.lock().expect("directory poisoned");
            self.install_page(&mut dir, page_id, true)?
        };
        let frame = &self.frames[idx];
        let content = frame.content.write().expect("content latch poisoned");
        Ok(PageWriteGuard {
            frame,
            pool: self,
            content: Some(content),
            page_id,
        })
    }

    /// Flush all dirty pages with `page_lsn <= up_to`. Used by checkpoints.
    /// Skips pages that are momentarily latched (fuzzy checkpoint behavior).
    pub fn flush_through(&self, up_to: Lsn) -> Result<()> {
        self.flush_matching(|lsn| lsn <= up_to)
    }

    /// Flush every dirty page, then sync the heap file. Used at clean shutdown.
    pub fn flush_all(&self) -> Result<()> {
        self.flush_matching(|_| true)?;
        self.disk.sync()?;
        Ok(())
    }

    // ── Internals ───────────────────────────────────────────────────────

    /// Resolve `page_id` to a pinned frame index (cache hit or miss+load).
    fn pin_for_access(&self, page_id: PageId) -> Result<usize> {
        let mut dir = self.dir.lock().expect("directory poisoned");
        if let Some(&idx) = dir.page_table.get(&page_id) {
            let frame = &self.frames[idx];
            let mut s = frame.state.lock().expect("frame state poisoned");
            if s.page_id == Some(page_id) {
                s.pin_count += 1;
                s.usage_count = (s.usage_count + 1).min(MAX_USAGE);
                return Ok(idx);
            }
        }
        self.install_page(&mut dir, page_id, false)
    }

    /// Seat `page_id` into a victim frame (evicting if necessary) and pin it.
    /// Caller must hold the directory lock.
    fn install_page(&self, dir: &mut Directory, page_id: PageId, fresh: bool) -> Result<usize> {
        let idx = self.find_victim(dir)?;
        let frame = &self.frames[idx];

        {
            let mut content = frame.content.write().expect("content latch poisoned");
            if fresh {
                content.fill(0);
            } else {
                self.disk.read_page(page_id, &mut content)?;
            }
        }

        let page_lsn = if fresh {
            Lsn::ZERO
        } else {
            let content = frame.content.read().expect("content latch poisoned");
            Lsn(u64::from_le_bytes(
                content[0..8].try_into().expect("8 bytes"),
            ))
        };

        {
            let mut s = frame.state.lock().expect("frame state poisoned");
            s.page_id = Some(page_id);
            s.pin_count = 1;
            s.usage_count = 1;
            s.dirty = false;
            s.page_lsn = page_lsn;
        }
        dir.page_table.insert(page_id, idx);
        Ok(idx)
    }

    /// Clock-sweep for an evictable frame, flushing it if dirty. Returns a
    /// cleared frame index (its `page_id` is `None`). Caller holds the dir lock.
    fn find_victim(&self, dir: &mut Directory) -> Result<usize> {
        let n = self.frames.len();
        let mut visits = 0usize;
        loop {
            if visits >= 4 * n {
                return Err(BufferError::Exhausted { frames: n });
            }
            let idx = dir.clock_hand;
            dir.clock_hand = (idx + 1) % n;
            visits += 1;

            let frame = &self.frames[idx];
            let (old, dirty, page_lsn) = {
                let mut s = frame.state.lock().expect("frame state poisoned");
                if s.pin_count > 0 {
                    continue;
                }
                if s.usage_count > 0 {
                    s.usage_count -= 1;
                    continue;
                }
                (s.page_id, s.dirty, s.page_lsn)
            };

            // The frame is unpinned with usage 0: our victim. Flush if dirty.
            if dirty {
                if let Some(old_id) = old {
                    match frame.content.try_read() {
                        Ok(content) => {
                            // WAL invariant: log durable through page_lsn first.
                            self.wal.flush_through(page_lsn)?;
                            self.disk.write_page(old_id, &content)?;
                        }
                        // Momentarily latched despite pin 0 (a guard mid-drop):
                        // leave it and try the next frame.
                        Err(_) => continue,
                    }
                }
            }
            if let Some(old_id) = old {
                dir.page_table.remove(&old_id);
                dir.dirty.remove(&old_id);
            }
            let mut s = frame.state.lock().expect("frame state poisoned");
            s.page_id = None;
            s.dirty = false;
            s.usage_count = 0;
            s.page_lsn = Lsn::ZERO;
            return Ok(idx);
        }
    }

    fn flush_matching(&self, pred: impl Fn(Lsn) -> bool) -> Result<()> {
        let candidates: Vec<usize> = {
            let dir = self.dir.lock().expect("directory poisoned");
            dir.dirty
                .iter()
                .filter_map(|pid| dir.page_table.get(pid).copied())
                .collect()
        };
        for idx in candidates {
            let frame = &self.frames[idx];
            let (page_id, page_lsn, dirty) = {
                let s = frame.state.lock().expect("frame state poisoned");
                (s.page_id, s.page_lsn, s.dirty)
            };
            let Some(page_id) = page_id else { continue };
            if !dirty || !pred(page_lsn) {
                continue;
            }
            // Skip pages currently latched (fuzzy: they'll be flushed later).
            let Ok(content) = frame.content.try_read() else {
                continue;
            };
            self.wal.flush_through(page_lsn)?;
            self.disk.write_page(page_id, &content)?;
            drop(content);

            let mut dir = self.dir.lock().expect("directory poisoned");
            let mut s = frame.state.lock().expect("frame state poisoned");
            if s.page_id == Some(page_id) && s.page_lsn == page_lsn && s.dirty {
                s.dirty = false;
                dir.dirty.remove(&page_id);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn debug_total_pins(&self) -> u32 {
        self.frames
            .iter()
            .map(|f| f.state.lock().expect("state").pin_count)
            .sum()
    }
}

fn unpin(frame: &Frame) {
    let mut s = frame.state.lock().expect("frame state poisoned");
    debug_assert!(s.pin_count > 0, "unpin of an unpinned frame");
    s.pin_count -= 1;
}

/// RAII read guard. Holds a content read latch and a pin; releases both on drop.
pub struct PageReadGuard<'a> {
    frame: &'a Frame,
    content: RwLockReadGuard<'a, Box<[u8; PAGE_SIZE]>>,
}

impl PageReadGuard<'_> {
    /// The page this guard refers to.
    pub fn page_id(&self) -> PageId {
        self.frame
            .state
            .lock()
            .expect("frame state poisoned")
            .page_id
            .expect("pinned frame has a page")
    }
}

impl Deref for PageReadGuard<'_> {
    type Target = [u8; PAGE_SIZE];
    fn deref(&self) -> &Self::Target {
        &self.content
    }
}

impl Drop for PageReadGuard<'_> {
    fn drop(&mut self) {
        // The content read latch (a field) releases after this runs.
        unpin(self.frame);
    }
}

/// RAII write guard. Holds an exclusive content latch and a pin; on drop it
/// marks the frame dirty, releases the latch, and unpins.
pub struct PageWriteGuard<'a> {
    frame: &'a Frame,
    pool: &'a BufferPool,
    content: Option<RwLockWriteGuard<'a, Box<[u8; PAGE_SIZE]>>>,
    page_id: PageId,
}

impl PageWriteGuard<'_> {
    /// The page this guard refers to.
    pub fn page_id(&self) -> PageId {
        self.page_id
    }

    /// Record that the WAL record at `lsn` describes this page's latest change.
    ///
    /// Writes `lsn` into the page header (bytes 0..8) and updates the frame's
    /// tracked `page_lsn`, so eviction flushes the WAL through it first. Callers
    /// must call this after appending the modification's WAL record, before the
    /// guard is dropped.
    pub fn set_page_lsn(&mut self, lsn: Lsn) {
        let content = self.content.as_mut().expect("write latch held");
        content[0..8].copy_from_slice(&lsn.as_u64().to_le_bytes());
        let mut s = self.frame.state.lock().expect("frame state poisoned");
        if lsn > s.page_lsn {
            s.page_lsn = lsn;
        }
    }
}

impl Deref for PageWriteGuard<'_> {
    type Target = [u8; PAGE_SIZE];
    fn deref(&self) -> &Self::Target {
        self.content.as_ref().expect("write latch held")
    }
}

impl DerefMut for PageWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.content.as_mut().expect("write latch held")
    }
}

impl Drop for PageWriteGuard<'_> {
    fn drop(&mut self) {
        // 1. Mark dirty while still pinned, so eviction can't seat a clean
        //    frame before our change is recorded as needing a flush.
        {
            let mut dir = self.pool.dir.lock().expect("directory poisoned");
            let mut s = self.frame.state.lock().expect("frame state poisoned");
            s.dirty = true;
            dir.dirty.insert(self.page_id);
        }
        // 2. Release the content write latch before unpinning, so an evictor
        //    that sees pin_count == 0 can immediately read the page.
        self.content.take();
        // 3. Unpin.
        unpin(self.frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_wal::record::RecordPayload;
    use prism_wal::{Config as WalConfig, LogRecord, SyncMode};
    use proptest::collection::vec;
    use proptest::prelude::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let mut p = std::env::temp_dir();
            p.push(format!(
                "prism-buffer-{tag}-{}-{n}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Env {
        pool: BufferPool,
        wal: Arc<Wal>,
        _disk: Arc<DiskManager>,
        _tmp: TempDir,
    }

    impl Env {
        fn new(frames: usize) -> Self {
            let tmp = TempDir::new("env");
            let disk = Arc::new(DiskManager::open(&tmp.path().join("heap.db"), true).unwrap());
            let wal = Arc::new(
                Wal::open(
                    &tmp.path().join("wal"),
                    WalConfig {
                        segment_size: 64 * 1024,
                        sync_mode: SyncMode::None,
                    },
                )
                .unwrap(),
            );
            let pool = BufferPool::new(
                disk.clone(),
                wal.clone(),
                Config {
                    frame_count: frames,
                },
            )
            .unwrap();
            Env {
                pool,
                wal,
                _disk: disk,
                _tmp: tmp,
            }
        }
    }

    #[test]
    fn write_then_read_roundtrip() {
        let env = Env::new(8);
        let pid = {
            let mut g = env.pool.new_page().unwrap();
            g.fill(0xAB);
            g.page_id()
        };
        let g = env.pool.fetch_read(pid).unwrap();
        assert!(g.iter().all(|&b| b == 0xAB));
        drop(g);
        assert_eq!(env.pool.debug_total_pins(), 0);
    }

    #[test]
    fn dirty_page_survives_eviction() {
        // One frame: the second page forces the first to be evicted (flushed).
        let env = Env::new(1);
        let a = {
            let mut g = env.pool.new_page().unwrap();
            g.fill(0x11);
            g.page_id()
        };
        let _b = {
            let mut g = env.pool.new_page().unwrap();
            g.fill(0x22);
            g.page_id()
        };
        // `a` was evicted to disk; fetching it reloads and re-evicts `b`.
        let g = env.pool.fetch_read(a).unwrap();
        assert!(
            g.iter().all(|&b| b == 0x11),
            "evicted page must reload intact"
        );
        drop(g);
        assert_eq!(env.pool.debug_total_pins(), 0);
    }

    #[test]
    fn eviction_enforces_wal_invariant() {
        let env = Env::new(1);
        // Append a real WAL record; its LSN becomes the page's page_lsn.
        let lsn = env
            .wal
            .append(LogRecord::txn(
                1,
                Lsn::ZERO,
                RecordPayload::Commit {
                    commit_micros: 0,
                    flags: 0,
                },
            ))
            .unwrap();
        assert!(env.wal.durable_lsn() < lsn, "precondition: not yet durable");

        {
            let mut g = env.pool.new_page().unwrap();
            g.fill(0x33);
            g.set_page_lsn(lsn);
        } // dropped: dirty, page_lsn = lsn

        // Force eviction of the dirty page by seating another page.
        let _evictor = env.pool.new_page().unwrap();

        assert!(
            env.wal.durable_lsn() >= lsn,
            "WAL must be durable through page_lsn before the page hit disk"
        );
    }

    #[test]
    fn exhaustion_when_all_pinned() {
        let env = Env::new(2);
        let _g1 = env.pool.new_page().unwrap();
        let _g2 = env.pool.new_page().unwrap();
        match env.pool.new_page() {
            Err(BufferError::Exhausted { frames: 2 }) => {}
            other => panic!("expected Exhausted, got {:?}", other.err()),
        }
    }

    #[test]
    fn flush_all_persists_to_disk() {
        let env = Env::new(4);
        let mut ids = Vec::new();
        for i in 0..3u8 {
            let mut g = env.pool.new_page().unwrap();
            g.fill(0x40 + i);
            ids.push(g.page_id());
        }
        env.pool.flush_all().unwrap();
        // After flush, re-reading still returns the data (now also on disk).
        for (i, &pid) in ids.iter().enumerate() {
            let g = env.pool.fetch_read(pid).unwrap();
            assert!(g.iter().all(|&b| b == 0x40 + i as u8));
        }
    }

    proptest! {
        /// With more pages than frames, every page must read back exactly what
        /// was written — proving eviction+reload preserves data — and all pins
        /// must settle to zero.
        #[test]
        fn pages_survive_eviction_under_pressure(reads in vec(0u8..8, 1..300)) {
            let env = Env::new(3); // 3 frames, 8 pages => heavy eviction
            let mut ids = Vec::new();
            for i in 0..8u8 {
                let mut g = env.pool.new_page().unwrap();
                g.fill(i);
                ids.push(g.page_id());
            }
            for r in reads {
                let i = r as usize;
                let g = env.pool.fetch_read(ids[i]).unwrap();
                prop_assert!(g.iter().all(|&b| b == r));
            }
            prop_assert_eq!(env.pool.debug_total_pins(), 0);
        }
    }
}
