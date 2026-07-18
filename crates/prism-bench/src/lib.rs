//! `prism-bench` - synthetic workloads over the embedded engine.
//!
//! Drives the in-process [`Database`]/[`Session`] API (no network) and reports
//! throughput and latency percentiles. The workloads here are the testable core;
//! the binary parses arguments and prints the report. See
//! `docs/operations/benchmarking.md`.
//!
//! **Scope (this version):** four workloads - `kv` (a read/write mix), `sql`
//! (insert throughput), `doc` (insertOne throughput), and `xmodel` (one explicit
//! transaction per op touching SQL + document + KV). Requests go through
//! [`Session`] exactly as a network client's would, minus the socket. The full
//! TPC-C / YCSB suite and cross-tool comparisons in the spec are future work.

use std::sync::Arc;
use std::time::{Duration, Instant};

use prism_doc::{DocValue, Document};
use prism_protocol::{DocCommand, KvCommand, Message, TxnMode};
use prism_server::{Database, Session};

/// Workload parameters.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Number of measured operations (split across threads).
    pub ops: usize,
    /// Distinct keys per thread (KV workload).
    pub keys: usize,
    /// Worker threads.
    pub threads: usize,
    /// Percent of KV ops that are reads (0-100).
    pub read_pct: u8,
    /// KV value size in bytes.
    pub value_size: usize,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            ops: 50_000,
            keys: 10_000,
            threads: 1,
            read_pct: 50,
            value_size: 64,
        }
    }
}

/// A small, fast, non-cryptographic PRNG (SplitMix64) for key selection.
pub struct Rng(u64);

impl Rng {
    /// Seed the generator.
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }
    /// Next pseudo-random `u64`.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Latency summary over a set of operation durations.
#[derive(Clone, Copy, Debug)]
pub struct Summary {
    /// Number of samples.
    pub count: usize,
    /// Minimum latency.
    pub min: Duration,
    /// Median (p50).
    pub p50: Duration,
    /// 99th percentile.
    pub p99: Duration,
    /// 99.9th percentile.
    pub p999: Duration,
    /// Maximum latency.
    pub max: Duration,
    /// Arithmetic mean.
    pub mean: Duration,
}

impl Summary {
    /// Summarize a vector of latencies (consumes and sorts it). `None` if empty.
    pub fn from_latencies(mut samples: Vec<Duration>) -> Option<Summary> {
        if samples.is_empty() {
            return None;
        }
        samples.sort_unstable();
        let n = samples.len();
        let total: Duration = samples.iter().sum();
        Some(Summary {
            count: n,
            min: samples[0],
            p50: percentile(&samples, 50.0),
            p99: percentile(&samples, 99.0),
            p999: percentile(&samples, 99.9),
            max: samples[n - 1],
            mean: total / n as u32,
        })
    }
}

/// Nearest-rank percentile over an already-sorted slice.
fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(n - 1);
    sorted[idx]
}

/// The result of running one workload.
#[derive(Clone, Debug)]
pub struct BenchResult {
    /// Workload name.
    pub name: String,
    /// Operations measured.
    pub ops: usize,
    /// Wall-clock time over the measured phase.
    pub elapsed: Duration,
    /// Latency summary (`None` if no ops ran).
    pub summary: Option<Summary>,
}

impl BenchResult {
    /// Operations per second over the measured phase.
    pub fn ops_per_sec(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs > 0.0 {
            self.ops as f64 / secs
        } else {
            0.0
        }
    }
}

impl std::fmt::Display for BenchResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:<8} {:>10} ops  {:>9.0} ops/sec",
            self.name,
            self.ops,
            self.ops_per_sec()
        )?;
        if let Some(s) = &self.summary {
            write!(
                f,
                "   p50 {:>8}  p99 {:>8}  p99.9 {:>8}  (min {} / max {})",
                fmt_dur(s.p50),
                fmt_dur(s.p99),
                fmt_dur(s.p999),
                fmt_dur(s.min),
                fmt_dur(s.max),
            )?;
        }
        Ok(())
    }
}

/// Format a duration compactly (ns/µs/ms/s).
pub fn fmt_dur(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        format!("{:.1}µs", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.1}ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

/// Run a workload by name (`kv`, `sql`, `doc`, `xmodel`).
pub fn run(name: &str, db: &Arc<Database>, params: &Params) -> Result<BenchResult, String> {
    match name {
        "kv" => run_kv(db, params),
        "sql" => run_sql(db, params),
        "doc" => run_doc(db, params),
        "xmodel" => run_xmodel(db, params),
        other => Err(format!("unknown workload {other:?} (kv|sql|doc|xmodel)")),
    }
}

/// All workload names, in display order.
pub const WORKLOADS: [&str; 4] = ["kv", "sql", "doc", "xmodel"];

// ---- workloads ---------------------------------------------------------------

/// KV read/write mix. Each thread owns its own `keys` keys (prefixed by thread
/// id) so concurrent writers never touch the same key.
pub fn run_kv(db: &Arc<Database>, p: &Params) -> Result<BenchResult, String> {
    let threads = p.threads.max(1);
    let value = vec![b'x'; p.value_size];

    // Load each thread's keys (not measured).
    {
        let mut s = Session::new(db.clone());
        for t in 0..threads {
            for k in 0..p.keys {
                ok(s.handle(Message::KvOp {
                    namespace: "bench".into(),
                    command: KvCommand::Put {
                        key: kv_key(t, k),
                        value: value.clone(),
                    },
                }))?;
            }
        }
    }

    let per = p.ops / threads;
    let read_pct = p.read_pct as u64;
    let elapsed_and_lat = parallel(db, threads, move |t, db| {
        let mut s = Session::new(db);
        let mut rng = Rng::new(t as u64 + 1);
        let value = vec![b'x'; p.value_size];
        let mut lat = Vec::with_capacity(per);
        for _ in 0..per {
            let k = (rng.next_u64() as usize) % p.keys.max(1);
            let read = rng.next_u64() % 100 < read_pct;
            let command = if read {
                KvCommand::Get { key: kv_key(t, k) }
            } else {
                KvCommand::Put {
                    key: kv_key(t, k),
                    value: value.clone(),
                }
            };
            let start = Instant::now();
            ok(s.handle(Message::KvOp {
                namespace: "bench".into(),
                command,
            }))?;
            lat.push(start.elapsed());
        }
        Ok(lat)
    })?;
    Ok(finish("kv", elapsed_and_lat))
}

/// SQL insert throughput into one unindexed table.
pub fn run_sql(db: &Arc<Database>, p: &Params) -> Result<BenchResult, String> {
    {
        let mut s = Session::new(db.clone());
        ok(s.handle(sql("CREATE TABLE bench (id BIGINT NOT NULL, val BIGINT)")))?;
    }
    let threads = p.threads.max(1);
    let per = p.ops / threads;
    let elapsed_and_lat = parallel(db, threads, move |t, db| {
        let mut s = Session::new(db);
        let base = (t * per) as i64;
        let mut lat = Vec::with_capacity(per);
        for i in 0..per {
            let id = base + i as i64;
            let stmt = format!("INSERT INTO bench VALUES ({id}, {id})");
            let start = Instant::now();
            ok(s.handle(sql(&stmt)))?;
            lat.push(start.elapsed());
        }
        Ok(lat)
    })?;
    Ok(finish("sql", elapsed_and_lat))
}

/// Document insertOne throughput.
pub fn run_doc(db: &Arc<Database>, p: &Params) -> Result<BenchResult, String> {
    let threads = p.threads.max(1);
    let per = p.ops / threads;
    let elapsed_and_lat = parallel(db, threads, move |t, db| {
        let mut s = Session::new(db);
        let mut lat = Vec::with_capacity(per);
        for i in 0..per {
            let doc = Document::from_fields([
                ("seq".to_string(), DocValue::Int64((t * per + i) as i64)),
                ("name".to_string(), DocValue::Str("benchmark".into())),
            ]);
            let bytes = doc.encode().map_err(|e| e.to_string())?;
            let start = Instant::now();
            ok(s.handle(Message::DocOp {
                collection: "bench".into(),
                command: DocCommand::InsertOne(bytes),
            }))?;
            lat.push(start.elapsed());
        }
        Ok(lat)
    })?;
    Ok(finish("doc", elapsed_and_lat))
}

/// Cross-model: one explicit transaction per op, writing a SQL row, a document,
/// and a KV pair, then committing - the single-WAL cross-model path.
pub fn run_xmodel(db: &Arc<Database>, p: &Params) -> Result<BenchResult, String> {
    {
        let mut s = Session::new(db.clone());
        ok(s.handle(sql("CREATE TABLE bench (id BIGINT NOT NULL)")))?;
    }
    let threads = p.threads.max(1);
    let per = p.ops / threads;
    let elapsed_and_lat = parallel(db, threads, move |t, db| {
        let mut s = Session::new(db);
        let base = t * per;
        let mut lat = Vec::with_capacity(per);
        for i in 0..per {
            let n = (base + i) as i64;
            let doc = Document::from_fields([("acct".to_string(), DocValue::Int64(n))]);
            let doc_bytes = doc.encode().map_err(|e| e.to_string())?;
            let start = Instant::now();
            ok(s.handle(Message::Begin {
                mode: TxnMode::ReadWrite,
            }))?;
            ok(s.handle(sql(&format!("INSERT INTO bench VALUES ({n})"))))?;
            ok(s.handle(Message::DocOp {
                collection: "bench".into(),
                command: DocCommand::InsertOne(doc_bytes),
            }))?;
            ok(s.handle(Message::KvOp {
                namespace: "bench".into(),
                command: KvCommand::Put {
                    key: kv_key(t, i),
                    value: n.to_le_bytes().to_vec(),
                },
            }))?;
            ok(s.handle(Message::Commit { idempotency_key: 0 }))?;
            lat.push(start.elapsed());
        }
        Ok(lat)
    })?;
    Ok(finish("xmodel", elapsed_and_lat))
}

// ---- helpers -----------------------------------------------------------------

/// Run `body` on `threads` worker threads, returning the wall time and the
/// merged latencies. `body(thread_id, db)` returns that thread's latencies.
fn parallel<F>(
    db: &Arc<Database>,
    threads: usize,
    body: F,
) -> Result<(Duration, Vec<Duration>), String>
where
    F: Fn(usize, Arc<Database>) -> Result<Vec<Duration>, String> + Sync,
{
    let body = &body; // shared across threads (each captures a &F, which is Send)
    let mut merged = Vec::new();
    let start = Instant::now();
    let elapsed = std::thread::scope(|scope| -> Result<Duration, String> {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let db = db.clone();
                scope.spawn(move || body(t, db))
            })
            .collect();
        for h in handles {
            let lat = h
                .join()
                .map_err(|_| "worker thread panicked".to_string())??;
            merged.extend(lat);
        }
        Ok(start.elapsed())
    })?;
    Ok((elapsed, merged))
}

fn finish(name: &str, (elapsed, latencies): (Duration, Vec<Duration>)) -> BenchResult {
    BenchResult {
        name: name.to_string(),
        ops: latencies.len(),
        elapsed,
        summary: Summary::from_latencies(latencies),
    }
}

fn kv_key(thread: usize, k: usize) -> Vec<u8> {
    format!("t{thread}-k{k}").into_bytes()
}

fn sql(stmt: &str) -> Message {
    Message::SqlExecute {
        sql: stmt.to_string(),
        params: vec![],
        options: 1,
    }
}

/// Assert a response was successful, surfacing any error trailer.
fn ok(response: Message) -> Result<(), String> {
    match response {
        Message::Pong
        | Message::SqlResult { status: 0, .. }
        | Message::KvResult { status: 0, .. }
        | Message::DocResult { status: 0, .. }
        | Message::TxnAck { status: 0, .. } => Ok(()),
        Message::SqlResult { error, .. }
        | Message::KvResult { error, .. }
        | Message::DocResult { error, .. }
        | Message::TxnAck { error, .. } => Err(format!("operation failed: {error:?}")),
        other => Err(format!("unexpected response: {:?}", other.message_type())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_testkit::TempDir;

    fn db() -> (Arc<Database>, TempDir) {
        let tmp = TempDir::new("bench").unwrap();
        (Arc::new(Database::open(tmp.path()).unwrap()), tmp)
    }

    fn tiny() -> Params {
        Params {
            ops: 200,
            keys: 50,
            threads: 1,
            read_pct: 50,
            value_size: 16,
        }
    }

    #[test]
    fn percentiles_are_ordered() {
        let samples: Vec<Duration> = (1u64..=100).map(Duration::from_micros).collect();
        let s = Summary::from_latencies(samples).unwrap();
        assert_eq!(s.count, 100);
        assert_eq!(s.min, Duration::from_micros(1));
        assert_eq!(s.max, Duration::from_micros(100));
        assert!(s.min <= s.p50 && s.p50 <= s.p99 && s.p99 <= s.p999 && s.p999 <= s.max);
        assert_eq!(s.p50, Duration::from_micros(50));
        assert_eq!(s.p99, Duration::from_micros(99));
    }

    #[test]
    fn empty_summary_is_none() {
        assert!(Summary::from_latencies(vec![]).is_none());
    }

    #[test]
    fn each_workload_runs() {
        for name in WORKLOADS {
            // A fresh database per workload (sql and xmodel both create a table).
            let (db, _tmp) = db();
            let result = run(name, &db, &tiny()).unwrap();
            assert_eq!(result.name, name);
            assert_eq!(result.ops, 200, "{name} ran the requested op count");
            assert!(result.ops_per_sec() > 0.0);
            let s = result.summary.unwrap();
            assert!(s.p50 <= s.p99 && s.p99 <= s.max);
        }
    }

    #[test]
    fn kv_read_only_mix_runs() {
        let (db, _tmp) = db();
        let params = Params {
            read_pct: 100,
            ..tiny()
        };
        let result = run_kv(&db, &params).unwrap();
        assert_eq!(result.ops, 200);
    }

    #[test]
    fn multithreaded_splits_work() {
        let (db, _tmp) = db();
        let params = Params {
            ops: 200,
            threads: 4,
            ..tiny()
        };
        // 200 / 4 = 50 ops per thread => 200 total.
        let result = run_sql(&db, &params).unwrap();
        assert_eq!(result.ops, 200);
    }
}
