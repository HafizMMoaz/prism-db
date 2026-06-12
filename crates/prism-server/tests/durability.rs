//! Durability across restart: data written and committed in all three models is
//! still queryable by name after closing and reopening the same `Database`
//! directory — proving the catalog (table/collection/namespace → heap) and the
//! record store both recover.

use std::sync::Arc;

use prism_doc::{DocValue, Document};
use prism_protocol::{DocCommand, DocQuery, KvCommand, KvResultBody, Message, Value as WireValue};
use prism_server::{Config, Database, Session};
use prism_testkit::TempDir;

fn sql(s: &str) -> Message {
    Message::SqlExecute {
        sql: s.into(),
        params: vec![],
        options: 1,
    }
}

fn doc_insert(collection: &str, fields: &[(&str, DocValue)]) -> Message {
    let doc = Document::from_fields(fields.iter().map(|(k, v)| (k.to_string(), v.clone())));
    Message::DocOp {
        collection: collection.into(),
        command: DocCommand::InsertOne(doc.encode().unwrap()),
    }
}

fn to_wire(v: &DocValue) -> WireValue {
    match v {
        DocValue::Null => WireValue::Null,
        DocValue::Bool(b) => WireValue::Bool(*b),
        DocValue::Int32(n) => WireValue::Int32(*n),
        DocValue::Int64(n) => WireValue::Int64(*n),
        DocValue::Double(d) => WireValue::Double(*d),
        DocValue::Str(s) => WireValue::Str(s.clone()),
        DocValue::Timestamp(t) => WireValue::Timestamp(*t),
        DocValue::ObjectId(id) => WireValue::ObjectId(id.0),
    }
}

fn doc_find(collection: &str, query: &[(&str, DocValue)]) -> Message {
    let q = DocQuery::And(
        query
            .iter()
            .map(|(k, v)| DocQuery::Eq((*k).to_string(), to_wire(v)))
            .collect(),
    );
    Message::DocOp {
        collection: collection.into(),
        command: DocCommand::Find {
            query: q.to_bytes().unwrap(),
            options: vec![],
        },
    }
}

fn doc_find_all(collection: &str) -> Message {
    Message::DocOp {
        collection: collection.into(),
        command: DocCommand::Find {
            query: DocQuery::All.to_bytes().unwrap(),
            options: vec![],
        },
    }
}

fn kv(ns: &str, command: KvCommand) -> Message {
    Message::KvOp {
        namespace: ns.into(),
        command,
    }
}

#[test]
fn all_three_models_survive_restart() {
    let tmp = TempDir::new("durability").unwrap();

    // ---- Session 1: write committed data in every model, then close. ----
    {
        let db = Arc::new(Database::open(tmp.path()).unwrap());
        let mut s = Session::new(db.clone());
        s.handle(sql(
            "CREATE TABLE accounts (id BIGINT NOT NULL, owner TEXT)",
        ));
        s.handle(sql("INSERT INTO accounts VALUES (1,'alice'),(2,'bob')"));
        s.handle(doc_insert("audit", &[("acct", DocValue::Int64(1))]));
        s.handle(kv(
            "balances",
            KvCommand::Put {
                key: b"acct:1".to_vec(),
                value: b"100".to_vec(),
            },
        ));
        // drop session, then database (clean close — committed WAL is on disk)
        drop(s);
        drop(db);
    }

    // ---- Session 2: reopen the SAME directory; everything is still there. ----
    let db = Arc::new(Database::open(tmp.path()).unwrap());
    let mut s = Session::new(db);

    // SQL: the table schema and rows recovered (queryable by name).
    match s.handle(sql("SELECT id, owner FROM accounts")) {
        Message::SqlResult {
            status: 0, rows, ..
        } => {
            assert_eq!(rows.len(), 2, "both rows recovered");
            assert_eq!(rows[0][0], Some(WireValue::Int64(1)));
            assert_eq!(rows[0][1], Some(WireValue::Str("alice".into())));
        }
        other => panic!("expected SqlResult, got {other:?}"),
    }

    // Document: the collection's heap recovered, the document is found.
    match s.handle(doc_find_all("audit")) {
        Message::DocResult {
            status: 0, docs, ..
        } => {
            assert_eq!(docs.len(), 1, "document recovered");
            let doc = Document::decode(&docs[0]).unwrap();
            assert_eq!(doc.get("acct"), Some(&DocValue::Int64(1)));
        }
        other => panic!("expected DocResult, got {other:?}"),
    }

    // KV: the namespace recovered and its index rebuilt.
    match s.handle(kv(
        "balances",
        KvCommand::Get {
            key: b"acct:1".to_vec(),
        },
    )) {
        Message::KvResult {
            status: 0,
            body: KvResultBody::Get { value },
            ..
        } => assert_eq!(value.as_deref(), Some(&b"100"[..])),
        other => panic!("expected KvResult, got {other:?}"),
    }
}

#[test]
fn checkpoint_then_more_writes_all_survive_durable_restart() {
    // A durable database: write, checkpoint (flush to disk), write more, then
    // reopen. Both the checkpointed and the post-checkpoint data must be there.
    let tmp = TempDir::new("durability-ckpt").unwrap();
    {
        let db = Arc::new(Database::open_with(tmp.path(), Config::durable()).unwrap());
        let mut s = Session::new(db.clone());
        s.handle(sql("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT)"));
        s.handle(sql("INSERT INTO t VALUES (1,10),(2,20)"));
        db.checkpoint().unwrap(); // flush the first batch to disk

        // More writes after the checkpoint (only in the WAL + dirty buffers).
        s.handle(sql("INSERT INTO t VALUES (3,30)"));
        s.handle(sql("UPDATE t SET v = 99 WHERE id = 1"));
        drop(s);
        drop(db);
    }

    let db = Arc::new(Database::open_with(tmp.path(), Config::durable()).unwrap());
    let mut s = Session::new(db);
    match s.handle(sql("SELECT id, v FROM t ORDER BY id")) {
        Message::SqlResult {
            status: 0, rows, ..
        } => {
            assert_eq!(
                rows.len(),
                3,
                "checkpointed and post-checkpoint rows survive"
            );
            assert_eq!(
                rows[0][1],
                Some(WireValue::Int64(99)),
                "post-checkpoint UPDATE applied"
            );
            assert_eq!(
                rows[2][0],
                Some(WireValue::Int64(3)),
                "post-checkpoint INSERT applied"
            );
        }
        other => panic!("expected SqlResult, got {other:?}"),
    }
}

#[test]
fn new_objects_after_restart_do_not_collide() {
    let tmp = TempDir::new("durability-realloc").unwrap();
    {
        let db = Arc::new(Database::open(tmp.path()).unwrap());
        let mut s = Session::new(db.clone());
        s.handle(sql("CREATE TABLE t1 (id BIGINT NOT NULL)"));
        s.handle(sql("INSERT INTO t1 VALUES (10)"));
        s.handle(doc_insert("c1", &[("v", DocValue::Int64(1))]));
        s.handle(kv(
            "n1",
            KvCommand::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            },
        ));
        drop(s);
        drop(db);
    }

    let db = Arc::new(Database::open(tmp.path()).unwrap());
    let mut s = Session::new(db);
    // Create new objects post-restart; their heaps must not reuse recovered ones.
    s.handle(sql("CREATE TABLE t2 (id BIGINT NOT NULL)"));
    s.handle(sql("INSERT INTO t2 VALUES (20)"));
    s.handle(doc_insert("c2", &[("v", DocValue::Int64(2))]));
    s.handle(kv(
        "n2",
        KvCommand::Put {
            key: b"k".to_vec(),
            value: b"v2".to_vec(),
        },
    ));

    // Old and new data coexist correctly.
    for (table, want) in [("t1", 10i64), ("t2", 20)] {
        match s.handle(sql(&format!("SELECT id FROM {table}"))) {
            Message::SqlResult {
                status: 0, rows, ..
            } => assert_eq!(rows, vec![vec![Some(WireValue::Int64(want))]]),
            other => panic!("expected SqlResult for {table}, got {other:?}"),
        }
    }
    assert_eq!(doc_count(&mut s, "c1"), 1);
    assert_eq!(doc_count(&mut s, "c2"), 1);
}

fn doc_count(s: &mut Session, collection: &str) -> usize {
    match s.handle(doc_find_all(collection)) {
        Message::DocResult {
            status: 0, docs, ..
        } => docs.len(),
        other => panic!("expected DocResult, got {other:?}"),
    }
}

#[test]
fn sql_primary_key_index_survives_restart() {
    let tmp = TempDir::new("durability-pk").unwrap();
    {
        let db = Arc::new(Database::open(tmp.path()).unwrap());
        let mut s = Session::new(db.clone());
        s.handle(sql("CREATE TABLE acct (id BIGINT PRIMARY KEY, owner TEXT)"));
        s.handle(sql(
            "INSERT INTO acct VALUES (1,'alice'),(2,'bob'),(3,'carol')",
        ));
        drop(s);
        drop(db);
    }

    let db = Arc::new(Database::open(tmp.path()).unwrap());
    let mut s = Session::new(db);

    // The primary-key index reopened at its persisted root: an index seek
    // returns the right row after restart.
    match s.handle(sql("SELECT owner FROM acct WHERE id = 2")) {
        Message::SqlResult {
            status: 0, rows, ..
        } => assert_eq!(rows, vec![vec![Some(WireValue::Str("bob".into()))]]),
        other => panic!("expected SqlResult, got {other:?}"),
    }

    // The unique constraint still holds (the index was reloaded, not empty).
    match s.handle(sql("INSERT INTO acct VALUES (1,'dup')")) {
        Message::SqlResult { status, .. } => {
            assert_ne!(status, 0, "duplicate primary key rejected after restart")
        }
        other => panic!("expected SqlResult, got {other:?}"),
    }
}

#[test]
fn document_id_index_survives_restart() {
    let tmp = TempDir::new("durability-doc-id").unwrap();
    let id;
    {
        let db = Arc::new(Database::open(tmp.path()).unwrap());
        let mut s = Session::new(db.clone());
        // Insert with an explicit integer _id so we can seek it after restart.
        match s.handle(doc_insert(
            "people",
            &[
                ("_id", DocValue::Int64(7)),
                ("name", DocValue::Str("zoe".into())),
            ],
        )) {
            Message::DocResult { status: 0, .. } => {}
            other => panic!("expected DocResult, got {other:?}"),
        }
        id = DocValue::Int64(7);
        drop(s);
        drop(db);
    }

    let db = Arc::new(Database::open(tmp.path()).unwrap());
    let mut s = Session::new(db);

    // The _id index reopened at its persisted root: a seek finds the document.
    match s.handle(doc_find("people", &[("_id", id.clone())])) {
        Message::DocResult {
            status: 0, docs, ..
        } => {
            assert_eq!(docs.len(), 1, "document found by _id seek after restart");
            let doc = Document::decode(&docs[0]).unwrap();
            assert_eq!(doc.get("name"), Some(&DocValue::Str("zoe".into())));
        }
        other => panic!("expected DocResult, got {other:?}"),
    }

    // The _id unique constraint still holds after restart.
    match s.handle(doc_insert(
        "people",
        &[("_id", id), ("name", DocValue::Str("dup".into()))],
    )) {
        Message::DocResult { status, .. } => {
            assert_ne!(status, 0, "duplicate _id rejected after restart")
        }
        other => panic!("expected DocResult, got {other:?}"),
    }
}
