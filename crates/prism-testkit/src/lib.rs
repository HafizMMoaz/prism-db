//! `prism-testkit` - reusable test harnesses.
//!
//! Home for the fault-injection harness and shared fixtures used across crates.
//! Not published; not subject to backward compatibility. See
//! `docs/operations/testing-strategy.md` and `docs/operations/fault-injection.md`.
//!
//! The centerpiece is [`FaultyDisk`] (an [`prism_storage::IoBackend`] that
//! injects torn/lost/EIO writes and probes the WAL invariant) and
//! [`run_scenario`], an in-process crash simulator over the storage foundation.

pub mod fault;
pub mod rng;
pub mod scenario;
pub mod tempdir;

pub use fault::{FaultConfig, FaultHandle, FaultStats, FaultyDisk};
pub use rng::Rng;
pub use scenario::{CrashReport, run_scenario};
pub use tempdir::TempDir;
