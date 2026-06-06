//! `FaultyDisk` — an [`IoBackend`] shim that injects I/O faults and probes the
//! WAL invariant.
//!
//! It wraps a real backend and, per a seeded probability config, can:
//! - **tear** a page write (persist only the first K bytes, but report success),
//! - **lose** a page write (persist nothing, report success),
//! - return **EIO**,
//! - make `sync` a **no-op** (lose durability).
//!
//! When given a [`Wal`] handle it also asserts, on every full-page heap write,
//! that the page's `page_lsn` (bytes 0..8) does not exceed the WAL's durable
//! LSN — i.e. the buffer pool never writes a page ahead of the log. Violations
//! are counted (see [`FaultStats::violations`]).

use std::io;
use std::sync::{Arc, Mutex};

use prism_storage::{IoBackend, PAGE_SIZE};
use prism_wal::Wal;

use crate::rng::Rng;

/// Per-call fault probabilities. All zero/false by default (a faithful disk).
#[derive(Clone, Copy, Debug, Default)]
pub struct FaultConfig {
    /// Probability a write is torn (prefix persisted, success reported).
    pub torn_prob: f64,
    /// Probability a write is lost (nothing persisted, success reported).
    pub lost_prob: f64,
    /// Probability a write returns `EIO`.
    pub eio_prob: f64,
    /// If set, `sync` returns success without flushing (loses durability).
    pub fsync_noop: bool,
}

impl FaultConfig {
    /// Whether this config injects no faults at all.
    pub fn is_faultless(&self) -> bool {
        self.torn_prob == 0.0 && self.lost_prob == 0.0 && self.eio_prob == 0.0 && !self.fsync_noop
    }
}

/// A snapshot of fault-injection counters.
#[derive(Clone, Copy, Debug, Default)]
pub struct FaultStats {
    /// Total write_at calls observed.
    pub writes: u64,
    /// Writes torn (partial).
    pub torn: u64,
    /// Writes lost (dropped).
    pub lost: u64,
    /// WAL-invariant violations observed (should always be 0).
    pub violations: u64,
}

struct FaultState {
    rng: Rng,
    cfg: FaultConfig,
    crashed: bool,
    stats: FaultStats,
}

/// A cloneable handle to a [`FaultyDisk`]'s shared state, usable after the disk
/// has been moved into a `DiskManager`.
#[derive(Clone)]
pub struct FaultHandle(Arc<Mutex<FaultState>>);

impl FaultHandle {
    /// Current counters.
    pub fn stats(&self) -> FaultStats {
        self.0.lock().expect("fault state poisoned").stats
    }

    /// Simulate process death: from now on, writes vanish and `sync` is a no-op.
    pub fn crash(&self) {
        self.0.lock().expect("fault state poisoned").crashed = true;
    }
}

/// An [`IoBackend`] that injects faults into an inner backend.
pub struct FaultyDisk {
    inner: Box<dyn IoBackend>,
    state: Arc<Mutex<FaultState>>,
    wal: Option<Arc<Wal>>,
}

impl FaultyDisk {
    /// Wrap `inner` with the given fault config and seed.
    pub fn new(inner: Box<dyn IoBackend>, cfg: FaultConfig, seed: u64) -> Self {
        Self {
            inner,
            state: Arc::new(Mutex::new(FaultState {
                rng: Rng::new(seed),
                cfg,
                crashed: false,
                stats: FaultStats::default(),
            })),
            wal: None,
        }
    }

    /// As [`Self::new`], but also check the WAL invariant on heap page writes.
    pub fn with_wal(inner: Box<dyn IoBackend>, cfg: FaultConfig, seed: u64, wal: Arc<Wal>) -> Self {
        let mut d = Self::new(inner, cfg, seed);
        d.wal = Some(wal);
        d
    }

    /// A handle to this disk's shared state (clone before moving the disk).
    pub fn handle(&self) -> FaultHandle {
        FaultHandle(self.state.clone())
    }
}

enum Action {
    Normal,
    Eio,
    Lost,
    Torn(usize),
}

impl IoBackend for FaultyDisk {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read_at(offset, buf)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        let full_page = offset % PAGE_SIZE as u64 == 0 && buf.len() == PAGE_SIZE;

        let action = {
            let mut st = self.state.lock().expect("fault state poisoned");
            if st.crashed {
                return Ok(buf.len()); // post-crash writes never land
            }

            // WAL invariant: a heap page must not be written ahead of the log.
            if let Some(wal) = &self.wal {
                if full_page {
                    let page_lsn = u64::from_le_bytes(buf[0..8].try_into().expect("8 bytes"));
                    if page_lsn > wal.durable_lsn().as_u64() {
                        st.stats.violations += 1;
                    }
                }
            }

            st.stats.writes += 1;
            let (eio, lost, torn) = (st.cfg.eio_prob, st.cfg.lost_prob, st.cfg.torn_prob);
            if st.rng.chance(eio) {
                Action::Eio
            } else if full_page && st.rng.chance(lost) {
                st.stats.lost += 1;
                Action::Lost
            } else if full_page && st.rng.chance(torn) {
                let k = 1 + st.rng.below((buf.len() - 1) as u64) as usize;
                st.stats.torn += 1;
                Action::Torn(k)
            } else {
                Action::Normal
            }
        };

        match action {
            Action::Eio => Err(io::Error::other("injected EIO")),
            Action::Lost => Ok(buf.len()),
            Action::Torn(k) => {
                self.inner.write_at(offset, &buf[..k])?;
                Ok(buf.len()) // report the full length despite a partial write
            }
            Action::Normal => self.inner.write_at(offset, buf),
        }
    }

    fn sync(&self) -> io::Result<()> {
        let skip = {
            let st = self.state.lock().expect("fault state poisoned");
            st.cfg.fsync_noop || st.crashed
        };
        if skip { Ok(()) } else { self.inner.sync() }
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }

    fn size(&self) -> io::Result<u64> {
        self.inner.size()
    }
}
