//! Durability across restart: data written and committed in all three models is
//! still queryable by name after closing and reopening the same `Database`
//! directory — proving the catalog (table/collection/namespace → heap) and the
//! record store both recover.

use std::sync::Arc;

use prism_doc::{DocValue, Document};
use prism_protocol::{DocCommand, KvCommand, KvResultBody, Message, Value as WireValue};
use prism_server::{Database, Session};
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

fn doc_find_all(collection: &str) -> Message {
    Message::DocOp {
        collection: collection.into(),
        command: DocCommand::Find {
            query: Document::new().encode().unwrap(),
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
