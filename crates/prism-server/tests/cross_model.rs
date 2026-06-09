//! The M3 exit gate: **cross-model ACID**.
//!
//! These tests are the whole thesis of PrismDB in executable form — that SQL,
//! documents, and KV are not three databases bolted together but three faces of
//! *one* transactional engine. A single transaction reaches into all three
//! models at once, and the guarantees hold across every one of them together:
//!
//! - `commit_is_atomic_across_all_three_models` — one transaction inserts a SQL
//!   row, a document, and a KV pair; on commit all three become visible, and on
//!   abort none of them do. Atomicity spans the models, not just one engine.
//! - `crash_during_a_cross_model_txn_recovers_consistently` — a committed
//!   cross-model transaction survives a crash in full, while a transaction that
//!   was mid-flight at the crash (data on disk, no commit record) leaves *no*
//!   trace in *any* model. All-or-nothing, across models, across a restart.
//!
//! All three engines share one [`RecordStore`], one [`TxnManager`], one WAL, and
//! one buffer pool — so MVCC, locking, and recovery are shared, and these
//! properties fall out of the single engine rather than being coordinated
//! between separate ones.

use std::sync::Arc;

use prism_buffer::{BufferPool, Config as BufConfig};
use prism_core::recover;
use prism_core::store::{HeapId, RecordStore};
use prism_core::txn::{TxnHandle, TxnManager, TxnMode};
use prism_doc::{DocCollection, DocValue, Document, Filter};
use prism_kv::KvNamespace;
use prism_sql::{Outcome, SqlEngine, Value};
use prism_storage::{DiskManager, PageId};
use prism_testkit::TempDir;
use prism_wal::{Config as WalConfig, SyncMode, Wal};

/// Heaps for the document and KV namespaces. Kept disjoint from the SQL engine,
/// which allocates table heaps from 1000 up.
const DOC_HEAP: HeapId = HeapId(2);
const KV_HEAP: HeapId = HeapId(1);

/// The three engines wired over one shared record store + transaction manager.
struct Models {
    sql: SqlEngine,
    docs: DocCollection,
    kv: KvNamespace,
    txns: Arc<TxnManager>,
}

/// Build the shared storage stack over `tmp`, then the three engines on top.
/// `create` controls whether the heap file is created fresh or reopened.
fn open_models(
    tmp: &TempDir,
    create: bool,
    recovered: Option<&prism_core::RecoveryReport>,
    kv_root: Option<PageId>,
    doc_root: Option<PageId>,
) -> Models {
    let disk = Arc::new(DiskManager::open(&tmp.path().join("heap.db"), create).unwrap());
    let wal = Arc::new(
        Wal::open(
            &tmp.path().join("wal"),
            WalConfig {
                segment_size: 256 * 1024,
                sync_mode: SyncMode::None,
            },
        )
        .unwrap(),
    );
    let buffer =
        Arc::new(BufferPool::new(disk, wal.clone(), BufConfig { frame_count: 64 }).unwrap());
    let txns = match recovered {
        Some(r) => Arc::new(TxnManager::new_recovered(
            wal.clone(),
            r.next_txn_id,
            &r.committed,
            &r.aborted,
        )),
        None => Arc::new(TxnManager::new(wal.clone())),
    };
    let store = Arc::new(RecordStore::new(buffer, wal, txns.clone()));
    if let Some(r) = recovered {
        store.seed_heap_directory(&r.heaps);
    }
    let kv = match kv_root {
        Some(root) => KvNamespace::open(store.clone(), KV_HEAP, root),
        None => KvNamespace::create(store.clone(), KV_HEAP).unwrap(),
    };
    let docs = match doc_root {
        Some(root) => DocCollection::open(store.clone(), DOC_HEAP, root),
        None => DocCollection::create(store.clone(), DOC_HEAP).unwrap(),
    };
    Models {
        sql: SqlEngine::new(store.clone(), txns.clone()),
        docs,
        kv,
        txns,
    }
}

/// Write one record to *each* model inside the single transaction `txn`.
/// `id` distinguishes the rows so each test can ask "did this one survive?".
fn write_all_three(m: &Models, txn: &TxnHandle, id: i64, tag: &str) {
    m.sql
        .execute(txn, &format!("INSERT INTO accounts VALUES ({id}, '{tag}')"))
        .unwrap();

    let mut d = Document::new();
    d.set("tag", DocValue::Str(tag.into()));
    d.set("acct", DocValue::Int64(id));
    m.docs.insert_one(txn, d).unwrap();

    m.kv.put(txn, format!("bal:{id}").as_bytes(), tag.as_bytes())
        .unwrap();
}

/// The SQL `id`s currently visible to `reader`.
fn sql_ids(m: &Models, reader: &TxnHandle) -> Vec<i64> {
    match m.sql.execute(reader, "SELECT id FROM accounts").unwrap() {
        Outcome::Select { rows, .. } => rows
            .iter()
            .map(|r| match r[0] {
                Value::Int64(n) => n,
                ref other => panic!("expected Int64, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Select, got {other:?}"),
    }
}

/// The `acct` values of documents whose `tag` matches, visible to `reader`.
fn doc_accts(m: &Models, reader: &TxnHandle, tag: &str) -> Vec<i64> {
    let found = m
        .docs
        .find(reader, &Filter::Eq("tag".into(), DocValue::Str(tag.into())))
        .unwrap();
    found
        .iter()
        .map(|d| match d.get("acct") {
            Some(DocValue::Int64(n)) => *n,
            other => panic!("expected acct Int64, got {other:?}"),
        })
        .collect()
}

#[test]
fn commit_is_atomic_across_all_three_models() {
    let tmp = TempDir::new("xmodel-commit").unwrap();
    let m = open_models(&tmp, true, None, None, None);
    m.sql
        .execute_autocommit("CREATE TABLE accounts (id BIGINT NOT NULL, owner TEXT)")
        .unwrap();

    // One transaction touches SQL + document + KV, then commits.
    let t1 = m.txns.begin(TxnMode::ReadWrite);
    write_all_three(&m, &t1, 1, "committed");
    t1.commit().unwrap();

    // A fresh reader sees the write in every model.
    let r = m.txns.begin(TxnMode::ReadOnly);
    assert_eq!(sql_ids(&m, &r), vec![1], "SQL row is visible after commit");
    assert_eq!(
        doc_accts(&m, &r, "committed"),
        vec![1],
        "document is visible after commit"
    );
    assert_eq!(
        m.kv.get(&r, b"bal:1").unwrap().as_deref(),
        Some(&b"committed"[..]),
        "KV pair is visible after commit"
    );
    r.commit().unwrap();

    // A second transaction touches all three models, then aborts.
    let t2 = m.txns.begin(TxnMode::ReadWrite);
    write_all_three(&m, &t2, 2, "rolledback");
    t2.abort().unwrap();

    // None of the aborted transaction's writes are visible — in any model —
    // while the earlier committed write is untouched.
    let r = m.txns.begin(TxnMode::ReadOnly);
    assert_eq!(sql_ids(&m, &r), vec![1], "aborted SQL row left no trace");
    assert!(
        doc_accts(&m, &r, "rolledback").is_empty(),
        "aborted document left no trace"
    );
    assert_eq!(
        m.kv.get(&r, b"bal:2").unwrap(),
        None,
        "aborted KV pair left no trace"
    );
    r.commit().unwrap();
}

#[test]
fn crash_during_a_cross_model_txn_recovers_consistently() {
    let tmp = TempDir::new("xmodel-crash").unwrap();

    // ---- Session 1: a committed cross-model txn, a loser, then a crash. ----
    let roots = {
        let m = open_models(&tmp, true, None, None, None);
        m.sql
            .execute_autocommit("CREATE TABLE accounts (id BIGINT NOT NULL, owner TEXT)")
            .unwrap();

        // T1: a complete cross-model transaction that commits durably.
        let t1 = m.txns.begin(TxnMode::ReadWrite);
        write_all_three(&m, &t1, 1, "committed");
        t1.commit().unwrap();

        // T2: a cross-model transaction that is *mid-flight* — its writes reach
        // all three models but it never commits (the "loser").
        let t2 = m.txns.begin(TxnMode::ReadWrite);
        write_all_three(&m, &t2, 2, "loser");

        // T3: a later committed write. Its group-commit flush forces T2's
        // already-appended data records out to the durable WAL — so recovery
        // genuinely has to replay the loser's records and then *hide* them,
        // rather than the loser conveniently never reaching disk.
        let t3 = m.txns.begin(TxnMode::ReadWrite);
        m.kv.put(&t3, b"sentinel", b"ok").unwrap();
        t3.commit().unwrap();

        // Crash: leave the block. The storage stack (`m`) and the still-open
        // loser `t2` are dropped without a clean flush. T2's `Drop` appends a
        // best-effort abort, but it is never flushed — so the durable WAL ends
        // at T3's commit, with T2's data records present but T2 uncommitted: a
        // genuine mid-flight loser for recovery to neutralize.
        let roots = (m.kv.index_root(), m.docs.index_root());
        let _ = &t2;
        roots
    };
    let (kv_root, doc_root) = roots;

    // ---- Recover the heap from the WAL. ----
    let report = {
        let wal = Wal::open(
            &tmp.path().join("wal"),
            WalConfig {
                segment_size: 256 * 1024,
                sync_mode: SyncMode::None,
            },
        )
        .unwrap();
        let disk = DiskManager::open(&tmp.path().join("heap.db"), false).unwrap();
        let r = recover(&wal, &disk).unwrap();
        disk.close().unwrap();
        r
    };

    // ---- Session 2: reopen seeded from recovery; verify every model. ----
    let m = open_models(&tmp, false, Some(&report), Some(kv_root), Some(doc_root));
    // Re-create the catalog so SELECT can find the table. The first table is
    // assigned heap 1000 again, matching the heap recovery rebuilt.
    m.sql
        .execute_autocommit("CREATE TABLE accounts (id BIGINT NOT NULL, owner TEXT)")
        .unwrap();

    // The KV index tree reopens at its persisted root (no rescan needed).
    let reader = m.txns.begin(TxnMode::ReadOnly);

    // The committed cross-model transaction survived — in full, in every model.
    assert_eq!(
        sql_ids(&m, &reader),
        vec![1],
        "committed SQL row survived; loser's did not"
    );
    assert_eq!(
        doc_accts(&m, &reader, "committed"),
        vec![1],
        "committed document survived"
    );
    assert_eq!(
        m.kv.get(&reader, b"bal:1").unwrap().as_deref(),
        Some(&b"committed"[..]),
        "committed KV pair survived"
    );
    assert_eq!(
        m.kv.get(&reader, b"sentinel").unwrap().as_deref(),
        Some(&b"ok"[..]),
        "the flushing committed write survived"
    );

    // The mid-flight transaction left no trace — in any model — even though its
    // records were on durable storage at the moment of the crash.
    assert!(
        doc_accts(&m, &reader, "loser").is_empty(),
        "loser document is invisible after recovery"
    );
    assert_eq!(
        m.kv.get(&reader, b"bal:2").unwrap(),
        None,
        "loser KV pair is invisible after recovery"
    );
    reader.commit().unwrap();
}
