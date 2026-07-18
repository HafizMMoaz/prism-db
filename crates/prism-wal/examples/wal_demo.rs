//! A runnable smoke of the WAL at its public boundary.
//!
//! Drives a real WAL the way a caller would: open in a temp dir, append a
//! transaction's worth of records, flush (fsync) through the commit, then
//! reopen from disk and replay - proving durability and recovery round-trip.
//!
//! Run with: `cargo run -p prism-wal --example wal_demo`

use std::path::PathBuf;

use prism_storage::PageId;
use prism_wal::record::RecordPayload;
use prism_wal::{Config, LogRecord, Lsn, SyncMode, Wal};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A throwaway directory for this demo run.
    let dir: PathBuf = std::env::temp_dir().join(format!("prism-wal-demo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let config = Config {
        segment_size: 64 * 1024,
        sync_mode: SyncMode::Fsync,
    };

    // ── Session 1: write a small transaction and commit it durably. ─────────
    let commit_lsn = {
        let wal = Wal::open(&dir, config)?;
        println!("opened WAL at {}", dir.display());

        let insert = wal.append(LogRecord::txn(
            42,
            Lsn::ZERO,
            RecordPayload::Insert {
                page_id: PageId(7),
                slot_id: 0,
                after_image: b"hello, durable world".to_vec(),
            },
        ))?;
        println!("appended Insert       -> {insert}");

        let commit = wal.append(LogRecord::txn(
            42,
            insert,
            RecordPayload::Commit {
                commit_micros: 1_700_000_000_000_000,
                flags: 0,
            },
        ))?;
        println!("appended Commit       -> {commit}");

        wal.flush_through(commit)?;
        println!("flushed; durable_lsn  =  {}", wal.durable_lsn());
        commit
    };

    // ── Session 2: reopen from disk and replay (simulates restart). ─────────
    println!("\n-- reopening (simulated restart) --");
    let wal = Wal::open(&dir, config)?;
    println!("recovered durable_lsn =  {}", wal.durable_lsn());
    assert!(
        wal.durable_lsn() >= commit_lsn,
        "committed record must survive restart"
    );

    println!("replaying from start:");
    let mut count = 0;
    for entry in wal.replay(Lsn::from_parts(0, 64)) {
        let (lsn, record) = entry?;
        let kind = match &record.payload {
            RecordPayload::Insert { after_image, .. } => {
                format!("Insert ({} bytes)", after_image.len())
            }
            RecordPayload::Commit { commit_micros, .. } => {
                format!("Commit (ts={commit_micros})")
            }
            other => format!("{other:?}"),
        };
        println!("  {lsn}  txn={}  {kind}", record.txn_id);
        count += 1;
    }
    println!("\nreplayed {count} record(s) - recovery round-trip OK");

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
