//! `prism-bench` — the benchmark harness binary.
//!
//! Runs synthetic workloads through the embedded engine and prints throughput
//! and latency percentiles. See `docs/operations/benchmarking.md`.
//!
//! Usage:
//! ```text
//! prism-bench [workload] [--ops N] [--keys N] [--threads N]
//!             [--read-pct P] [--value-size N] [--durable] [--data-dir DIR]
//! ```
//! `workload` is one of `kv`, `sql`, `doc`, `xmodel`, or `all` (default).
//! Without `--durable` the WAL does not fsync (in-memory speed); with it, every
//! commit fsyncs (the production setting), which dominates write latency.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use prism_bench::{Params, WORKLOADS, run};
use prism_server::{Config, Database};
use prism_wal::SyncMode;

struct Args {
    workload: String,
    params: Params,
    durable: bool,
    data_dir: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(message) => {
            eprintln!("prism-bench: {message}");
            return ExitCode::FAILURE;
        }
    };

    let config = Config {
        wal_sync: if args.durable {
            SyncMode::Fsync
        } else {
            SyncMode::None
        },
        ..Config::default()
    };

    eprintln!(
        "prism-bench: {} threads, {} ops, sync={}",
        args.params.threads,
        args.params.ops,
        if args.durable { "fsync" } else { "none" }
    );

    let workloads: Vec<&str> = if args.workload == "all" {
        WORKLOADS.to_vec()
    } else {
        vec![args.workload.as_str()]
    };

    let mut status = ExitCode::SUCCESS;
    for name in workloads {
        // Each workload gets its own fresh database so they don't interfere
        // (e.g. SQL and xmodel both create a `bench` table).
        match run_one(name, config, &args, name) {
            Ok(result) => println!("{result}"),
            Err(e) => {
                eprintln!("prism-bench: {name}: {e}");
                status = ExitCode::FAILURE;
            }
        }
    }
    status
}

/// Open a fresh database in a per-workload directory, run the workload, then
/// remove a scratch directory (a caller-supplied `--data-dir` is kept).
fn run_one(
    name: &str,
    config: Config,
    args: &Args,
    subdir: &str,
) -> Result<prism_bench::BenchResult, String> {
    let (base, temp) = match &args.data_dir {
        Some(d) => (d.clone(), false),
        None => (default_data_dir(), true),
    };
    let dir = base.join(subdir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let db = Arc::new(Database::open_with(&dir, config).map_err(|e| format!("open failed: {e}"))?);
    let result = run(name, &db, &args.params);

    drop(db);
    if temp {
        let _ = std::fs::remove_dir_all(&base);
    }
    result
}

fn parse_args() -> Result<Args, String> {
    let mut workload = "all".to_string();
    let mut params = Params::default();
    let mut durable = false;
    let mut data_dir = None;

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--ops" => params.ops = parse_next(&argv, &mut i, "--ops")?,
            "--keys" => params.keys = parse_next(&argv, &mut i, "--keys")?,
            "--threads" => params.threads = parse_next::<usize>(&argv, &mut i, "--threads")?.max(1),
            "--read-pct" => {
                let p: u8 = parse_next(&argv, &mut i, "--read-pct")?;
                params.read_pct = p.min(100);
            }
            "--value-size" => params.value_size = parse_next(&argv, &mut i, "--value-size")?,
            "--durable" => durable = true,
            "--data-dir" => {
                i += 1;
                data_dir = Some(PathBuf::from(
                    argv.get(i).ok_or("--data-dir needs a value")?,
                ));
            }
            "--help" | "-h" => return Err(USAGE.to_string()),
            w if !w.starts_with('-') => workload = w.to_string(),
            other => return Err(format!("unknown flag {other}\n{USAGE}")),
        }
        i += 1;
    }

    if workload != "all" && !WORKLOADS.contains(&workload.as_str()) {
        return Err(format!("unknown workload {workload:?}\n{USAGE}"));
    }
    Ok(Args {
        workload,
        params,
        durable,
        data_dir,
    })
}

fn parse_next<T: std::str::FromStr>(
    argv: &[String],
    i: &mut usize,
    flag: &str,
) -> Result<T, String> {
    *i += 1;
    argv.get(*i)
        .ok_or_else(|| format!("{flag} needs a value"))?
        .parse::<T>()
        .map_err(|_| format!("invalid value for {flag}"))
}

fn default_data_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("prism-bench-{}-{nanos}", std::process::id()))
}

const USAGE: &str = "usage: prism-bench [kv|sql|doc|xmodel|all] \
[--ops N] [--keys N] [--threads N] [--read-pct P] [--value-size N] \
[--durable] [--data-dir DIR]";
