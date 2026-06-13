//! End-to-end tests for the in-process dispatcher: protocol messages in,
//! protocol messages out, against a real shared engine — including a single
//! explicit transaction that spans all three models.

use std::sync::Arc;

use prism_doc::{DocValue, Document};
use prism_protocol::{
    AuthMechanism, DocCommand, DocQuery, DocUpdate, DocUpdateOp, KvCommand, KvResultBody, Message,
    TxnMode, Value as WireValue,
};
use prism_server::{Database, Session};
use prism_testkit::TempDir;

fn database() -> (Arc<Database>, TempDir) {
    let tmp = TempDir::new("server").unwrap();
    let db = Arc::new(Database::open(tmp.path()).unwrap());
    (db, tmp)
}

// ---- response accessors ------------------------------------------------------

fn sql_select(msg: Message) -> Vec<Vec<Option<WireValue>>> {
    match msg {
        Message::SqlResult {
            status,
            rows,
            error,
            ..
        } => {
            assert_eq!(status, 0, "sql error: {error:?}");
            rows
        }
        other => panic!("expected SqlResult, got {other:?}"),
    }
}

fn sql_affected(msg: Message) -> u64 {
    match msg {
        Message::SqlResult {
            status,
            affected_rows,
            error,
            ..
        } => {
            assert_eq!(status, 0, "sql error: {error:?}");
            affected_rows
        }
        other => panic!("expected SqlResult, got {other:?}"),
    }
}

fn kv_value(msg: Message) -> Option<Vec<u8>> {
    match msg {
        Message::KvResult {
            status,
            body: KvResultBody::Get { value },
            error,
        } => {
            assert_eq!(status, 0, "kv error: {error:?}");
            value
        }
        other => panic!("expected KvResult/Get, got {other:?}"),
    }
}

fn doc_results(msg: Message) -> Vec<Document> {
    match msg {
        Message::DocResult {
            status,
            docs,
            error,
            ..
        } => {
            assert_eq!(status, 0, "doc error: {error:?}");
            docs.iter().map(|b| Document::decode(b).unwrap()).collect()
        }
        other => panic!("expected DocResult, got {other:?}"),
    }
}

fn txn_ok(msg: Message) -> u64 {
    match msg {
        Message::TxnAck {
            status,
            txn_id,
            error,
            ..
        } => {
            assert_eq!(status, 0, "txn error: {error:?}");
            txn_id
        }
        other => panic!("expected TxnAck, got {other:?}"),
    }
}

// ---- message builders --------------------------------------------------------

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

/// Translate a document scalar to the protocol's wire scalar (for queries).
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

/// A flat `field = value [AND …]` query (empty ⇒ match all).
fn doc_find(collection: &str, query: &[(&str, DocValue)]) -> Message {
    let q = if query.is_empty() {
        DocQuery::All
    } else {
        DocQuery::And(
            query
                .iter()
                .map(|(k, v)| DocQuery::Eq((*k).to_string(), to_wire(v)))
                .collect(),
        )
    };
    doc_query(collection, q)
}

/// A document `Find` carrying an arbitrary [`DocQuery`].
fn doc_query(collection: &str, query: DocQuery) -> Message {
    Message::DocOp {
        collection: collection.into(),
        command: DocCommand::Find {
            query: query.to_bytes().unwrap(),
            options: vec![],
        },
    }
}

/// An `UpdateOne` carrying a structured query and update.
fn doc_update_one(collection: &str, query: DocQuery, update: DocUpdate) -> Message {
    Message::DocOp {
        collection: collection.into(),
        command: DocCommand::UpdateOne {
            query: query.to_bytes().unwrap(),
            update: update.to_bytes().unwrap(),
            options: vec![],
        },
    }
}

fn kv_put(ns: &str, key: &[u8], value: &[u8]) -> Message {
    Message::KvOp {
        namespace: ns.into(),
        command: KvCommand::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        },
    }
}

fn kv_get(ns: &str, key: &[u8]) -> Message {
    Message::KvOp {
        namespace: ns.into(),
        command: KvCommand::Get { key: key.to_vec() },
    }
}

// ---- tests -------------------------------------------------------------------

fn hello() -> Message {
    Message::Hello {
        protocol_version: prism_protocol::PROTOCOL_VERSION,
        client_name: "test".into(),
        client_version: "0".into(),
        features: 0,
    }
}

fn auth(username: &str, password: &str) -> Message {
    Message::Auth {
        mechanism: AuthMechanism::Password,
        username: username.into(),
        password: password.into(),
    }
}

#[test]
fn handshake_and_ping() {
    let (db, _tmp) = database();
    let mut s = Session::new_authenticating(db);

    assert!(matches!(
        s.handle(hello()),
        Message::HelloAck { status: 0, .. }
    ));
    assert!(matches!(
        s.handle(auth("admin", "admin")), // the default seeded account
        Message::AuthAck { status: 0, .. }
    ));
    assert!(!s.is_closing());
    assert!(matches!(s.handle(Message::Ping), Message::Pong));
}

#[test]
fn authentication_is_enforced() {
    let (db, _tmp) = database();

    // A query before the handshake is rejected and closes the connection.
    let mut early = Session::new_authenticating(db.clone());
    assert!(matches!(
        early.handle(sql("SELECT 1")),
        Message::Notice { .. }
    ));
    assert!(
        early.is_closing(),
        "a pre-handshake query closes the session"
    );

    // Wrong protocol version is rejected.
    let mut bad_version = Session::new_authenticating(db.clone());
    assert!(matches!(
        bad_version.handle(Message::Hello {
            protocol_version: 999,
            client_name: "x".into(),
            client_version: "0".into(),
            features: 0,
        }),
        Message::HelloAck { status, .. } if status != 0
    ));
    assert!(bad_version.is_closing());

    // Wrong password is rejected and closes the connection.
    let mut bad_pw = Session::new_authenticating(db.clone());
    bad_pw.handle(hello());
    assert!(matches!(
        bad_pw.handle(auth("admin", "wrong")),
        Message::AuthAck { status, .. } if status != 0
    ));
    assert!(bad_pw.is_closing());

    // A freshly added user can authenticate.
    db.add_user("svc", "s3cret").unwrap();
    let mut ok = Session::new_authenticating(db);
    ok.handle(hello());
    assert!(matches!(
        ok.handle(auth("svc", "s3cret")),
        Message::AuthAck { status: 0, .. }
    ));
    assert!(!ok.is_closing());
}

#[test]
fn sql_auto_commit_roundtrip() {
    let (db, _tmp) = database();
    let mut s = Session::new(db);

    s.handle(sql("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)"));
    assert_eq!(
        sql_affected(s.handle(sql("INSERT INTO users VALUES (1,'alice'),(2,'bob')"))),
        2
    );
    let rows = sql_select(s.handle(sql("SELECT id, name FROM users WHERE id = 1")));
    assert_eq!(
        rows,
        vec![vec![
            Some(WireValue::Int64(1)),
            Some(WireValue::Str("alice".into()))
        ]]
    );

    // UPDATE reports the affected count and the change is visible.
    assert_eq!(
        sql_affected(s.handle(sql("UPDATE users SET name = 'ALICE' WHERE id = 1"))),
        1
    );
    let rows = sql_select(s.handle(sql("SELECT name FROM users WHERE id = 1")));
    assert_eq!(rows, vec![vec![Some(WireValue::Str("ALICE".into()))]]);

    // DELETE reports the affected count and removes the row.
    assert_eq!(
        sql_affected(s.handle(sql("DELETE FROM users WHERE id = 2"))),
        1
    );
    assert!(sql_select(s.handle(sql("SELECT id FROM users WHERE id = 2"))).is_empty());

    // An aggregate flows back through the same result path: one row (id = 1)
    // remains after the update + delete above.
    let agg = sql_select(s.handle(sql("SELECT COUNT(*) FROM users")));
    assert_eq!(agg, vec![vec![Some(WireValue::Int64(1))]]);
}

#[test]
fn kv_auto_commit_roundtrip() {
    let (db, _tmp) = database();
    let mut s = Session::new(db);

    assert!(matches!(
        s.handle(kv_put("sessions", b"sid", b"payload")),
        Message::KvResult {
            status: 0,
            body: KvResultBody::Put,
            ..
        }
    ));
    assert_eq!(
        kv_value(s.handle(kv_get("sessions", b"sid"))).as_deref(),
        Some(&b"payload"[..])
    );
    assert_eq!(kv_value(s.handle(kv_get("sessions", b"missing"))), None);
}

#[test]
fn doc_auto_commit_roundtrip() {
    let (db, _tmp) = database();
    let mut s = Session::new(db);

    s.handle(doc_insert(
        "people",
        &[
            ("name", DocValue::Str("alice".into())),
            ("age", DocValue::Int64(30)),
        ],
    ));
    s.handle(doc_insert(
        "people",
        &[
            ("name", DocValue::Str("bob".into())),
            ("age", DocValue::Int64(25)),
        ],
    ));

    let found = doc_results(s.handle(doc_find(
        "people",
        &[("name", DocValue::Str("alice".into()))],
    )));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].get("age"), Some(&DocValue::Int64(30)));

    // Empty query matches all.
    let all = doc_results(s.handle(doc_find("people", &[])));
    assert_eq!(all.len(), 2);
}

#[test]
fn doc_query_operators_over_the_wire() {
    let (db, _tmp) = database();
    let mut s = Session::new(db);

    for (name, age, city) in [
        ("alice", 30, Some("NYC")),
        ("bob", 25, Some("LA")),
        ("carol", 40, Some("NYC")),
        ("dave", 35, None),
    ] {
        let mut fields = vec![
            ("name", DocValue::Str(name.into())),
            ("age", DocValue::Int64(age)),
        ];
        if let Some(c) = city {
            fields.push(("city", DocValue::Str(c.into())));
        }
        s.handle(doc_insert("people", &fields));
    }

    let names = |msg: Message| -> Vec<String> {
        let mut out: Vec<String> = doc_results(msg)
            .iter()
            .map(|d| match d.get("name") {
                Some(DocValue::Str(s)) => s.clone(),
                other => panic!("expected a name string, got {other:?}"),
            })
            .collect();
        out.sort();
        out
    };

    // $gt over the wire.
    let q = DocQuery::Gt("age".into(), WireValue::Int64(30));
    assert_eq!(
        names(s.handle(doc_query("people", q))),
        vec!["carol", "dave"]
    );

    // $in.
    let q = DocQuery::In(
        "name".into(),
        vec![WireValue::Str("alice".into()), WireValue::Str("bob".into())],
    );
    assert_eq!(
        names(s.handle(doc_query("people", q))),
        vec!["alice", "bob"]
    );

    // Boolean composition: (age <= 30) OR (city == "NYC").
    let q = DocQuery::Or(vec![
        DocQuery::Lte("age".into(), WireValue::Int64(30)),
        DocQuery::Eq("city".into(), WireValue::Str("NYC".into())),
    ]);
    assert_eq!(
        names(s.handle(doc_query("people", q))),
        vec!["alice", "bob", "carol"]
    );

    // $exists: only documents that have a "city" field.
    let q = DocQuery::Exists("city".into(), true);
    assert_eq!(
        names(s.handle(doc_query("people", q))),
        vec!["alice", "bob", "carol"]
    );

    // NOT (age > 30) keeps the 25/30 year olds.
    let q = DocQuery::Not(Box::new(DocQuery::Gt("age".into(), WireValue::Int64(30))));
    assert_eq!(
        names(s.handle(doc_query("people", q))),
        vec!["alice", "bob"]
    );
}

#[test]
fn doc_update_operators_over_the_wire() {
    let (db, _tmp) = database();
    let mut s = Session::new(db);

    s.handle(doc_insert(
        "u",
        &[
            ("name", DocValue::Str("alice".into())),
            ("visits", DocValue::Int64(1)),
            ("temp", DocValue::Int64(9)),
        ],
    ));

    // $set name, $inc visits by 5, $unset temp — all in one update.
    let q = DocQuery::Eq("name".into(), WireValue::Str("alice".into()));
    let upd = DocUpdate {
        ops: vec![
            DocUpdateOp::Set("name".into(), WireValue::Str("alicia".into())),
            DocUpdateOp::Inc("visits".into(), 5),
            DocUpdateOp::Unset("temp".into()),
        ],
    };
    match s.handle(doc_update_one("u", q, upd)) {
        Message::DocResult {
            status: 0,
            affected,
            ..
        } => assert_eq!(affected, 1),
        other => panic!("expected DocResult, got {other:?}"),
    }

    let docs = doc_results(s.handle(doc_query("u", DocQuery::All)));
    assert_eq!(docs.len(), 1);
    let d = &docs[0];
    assert_eq!(
        d.get("name"),
        Some(&DocValue::Str("alicia".into())),
        "$set applied"
    );
    assert_eq!(d.get("visits"), Some(&DocValue::Int64(6)), "$inc applied");
    assert_eq!(d.get("temp"), None, "$unset removed the field");
}

#[test]
fn explicit_transaction_spans_all_three_models() {
    let (db, _tmp) = database();
    let mut writer = Session::new(db.clone());

    // Set up the SQL table (auto-commit), then run one explicit transaction that
    // writes to SQL, document, and KV together.
    writer.handle(sql(
        "CREATE TABLE accounts (id BIGINT NOT NULL, owner TEXT)",
    ));

    txn_ok(writer.handle(Message::Begin {
        mode: TxnMode::ReadWrite,
    }));
    assert!(writer.in_transaction());
    sql_affected(writer.handle(sql("INSERT INTO accounts VALUES (1,'alice')")));
    writer.handle(doc_insert("audit", &[("acct", DocValue::Int64(1))]));
    writer.handle(kv_put("balances", b"acct:1", b"100"));

    // Before commit, a separate session sees none of it.
    let mut reader = Session::new(db.clone());
    assert!(sql_select(reader.handle(sql("SELECT id FROM accounts"))).is_empty());
    assert_eq!(kv_value(reader.handle(kv_get("balances", b"acct:1"))), None);
    assert!(doc_results(reader.handle(doc_find("audit", &[]))).is_empty());

    txn_ok(writer.handle(Message::Commit { idempotency_key: 0 }));
    assert!(!writer.in_transaction());

    // After commit, a fresh reader sees all three writes.
    let mut after = Session::new(db);
    assert_eq!(
        sql_select(after.handle(sql("SELECT id FROM accounts"))).len(),
        1
    );
    assert_eq!(
        kv_value(after.handle(kv_get("balances", b"acct:1"))).as_deref(),
        Some(&b"100"[..])
    );
    assert_eq!(doc_results(after.handle(doc_find("audit", &[]))).len(), 1);
}

#[test]
fn aborted_transaction_leaves_no_trace_in_any_model() {
    let (db, _tmp) = database();
    let mut writer = Session::new(db.clone());
    writer.handle(sql("CREATE TABLE t (id BIGINT NOT NULL)"));

    writer.handle(Message::Begin {
        mode: TxnMode::ReadWrite,
    });
    sql_affected(writer.handle(sql("INSERT INTO t VALUES (99)")));
    writer.handle(doc_insert("scratch", &[("v", DocValue::Int64(99))]));
    writer.handle(kv_put("scratch", b"k", b"v"));
    txn_ok(writer.handle(Message::Abort));

    let mut reader = Session::new(db);
    assert!(sql_select(reader.handle(sql("SELECT id FROM t"))).is_empty());
    assert_eq!(kv_value(reader.handle(kv_get("scratch", b"k"))), None);
    assert!(doc_results(reader.handle(doc_find("scratch", &[]))).is_empty());
}

#[test]
fn errors_are_reported_with_a_trailer() {
    let (db, _tmp) = database();
    let mut s = Session::new(db);

    // SELECT from a missing table.
    match s.handle(sql("SELECT * FROM nope")) {
        Message::SqlResult { status, error, .. } => {
            assert_ne!(status, 0);
            assert!(error.is_some(), "error trailer present");
        }
        other => panic!("expected SqlResult, got {other:?}"),
    }

    // SQL params are not yet supported.
    let with_param = Message::SqlExecute {
        sql: "SELECT 1".into(),
        params: vec![WireValue::Int64(1)],
        options: 0,
    };
    assert!(matches!(
        s.handle(with_param),
        Message::SqlResult { status: 1, .. }
    ));

    // KV range is unsupported on a hash namespace.
    let range = Message::KvOp {
        namespace: "n".into(),
        command: KvCommand::Range {
            start: b"a".to_vec(),
            end: b"z".to_vec(),
            max_results: 10,
        },
    };
    assert!(matches!(
        s.handle(range),
        Message::KvResult { status: 1, .. }
    ));
}

#[test]
fn idempotent_commit_dedupes_and_discards_the_retry() {
    let (db, _tmp) = database();
    {
        let mut s = Session::new(db.clone());
        s.handle(sql("CREATE TABLE t (id BIGINT NOT NULL)"));
    }
    let key = 0xABCD_u128; // a non-zero idempotency key

    // First transaction: insert 1, commit with the key.
    let mut s1 = Session::new(db.clone());
    s1.handle(Message::Begin {
        mode: TxnMode::ReadWrite,
    });
    sql_affected(s1.handle(sql("INSERT INTO t VALUES (1)")));
    let original_txn = txn_ok(s1.handle(Message::Commit {
        idempotency_key: key,
    }));

    // A "retry": a fresh transaction does different work but commits with the
    // SAME key. It must be de-duplicated — the original outcome is replayed and
    // this transaction's write is discarded.
    let mut s2 = Session::new(db.clone());
    s2.handle(Message::Begin {
        mode: TxnMode::ReadWrite,
    });
    sql_affected(s2.handle(sql("INSERT INTO t VALUES (2)")));
    match s2.handle(Message::Commit {
        idempotency_key: key,
    }) {
        Message::TxnAck {
            status: 0, txn_id, ..
        } => assert_eq!(txn_id, original_txn, "the original txn_id is replayed"),
        other => panic!("expected TxnAck, got {other:?}"),
    }

    // Only row 1 survives; the duplicate retry's row 2 was rolled back.
    let mut reader = Session::new(db);
    assert_eq!(
        sql_select(reader.handle(sql("SELECT id FROM t"))),
        vec![vec![Some(WireValue::Int64(1))]],
        "the retry's write was discarded"
    );
}

#[test]
fn dropping_a_session_aborts_its_open_transaction() {
    let (db, _tmp) = database();
    {
        let mut s = Session::new(db.clone());
        s.handle(sql("CREATE TABLE t (id BIGINT NOT NULL)"));
        s.handle(Message::Begin {
            mode: TxnMode::ReadWrite,
        });
        sql_affected(s.handle(sql("INSERT INTO t VALUES (1)")));
        // `s` is dropped here with the transaction still open.
    }
    let mut reader = Session::new(db);
    assert!(
        sql_select(reader.handle(sql("SELECT id FROM t"))).is_empty(),
        "the dropped session's open transaction was rolled back"
    );
}

#[test]
fn cannot_begin_twice_or_commit_without_a_transaction() {
    let (db, _tmp) = database();
    let mut s = Session::new(db);

    // Commit with no open transaction → error.
    assert!(matches!(
        s.handle(Message::Commit { idempotency_key: 0 }),
        Message::TxnAck { status: 1, .. }
    ));

    txn_ok(s.handle(Message::Begin {
        mode: TxnMode::ReadWrite,
    }));
    // A second Begin while one is open → error, and the first stays open.
    assert!(matches!(
        s.handle(Message::Begin {
            mode: TxnMode::ReadWrite
        }),
        Message::TxnAck { status: 1, .. }
    ));
    assert!(s.in_transaction());
    txn_ok(s.handle(Message::Abort));
}
