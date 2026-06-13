//! Per-session authorization: privileges are enforced for SQL, document, and KV
//! operations, and GRANT/REVOKE/CREATE USER take effect live.

use std::sync::Arc;

use prism_doc::{DocValue, Document};
use prism_protocol::{AuthMechanism, DocCommand, DocQuery, KvCommand, Message, PROTOCOL_VERSION};
use prism_server::{Database, Session};

fn database() -> Arc<Database> {
    let tmp = prism_testkit::TempDir::new("authz").unwrap();
    // Leak the temp dir for the test's lifetime (process exits after).
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    Arc::new(Database::open(&path).unwrap())
}

fn hello() -> Message {
    Message::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "test".into(),
        client_version: "0".into(),
        features: 0,
        database: String::new(),
    }
}

fn auth(user: &str, pw: &str) -> Message {
    Message::Auth {
        mechanism: AuthMechanism::Password,
        username: user.into(),
        password: pw.into(),
    }
}

fn sql(s: &str) -> Message {
    Message::SqlExecute {
        sql: s.into(),
        params: vec![],
        options: 1,
    }
}

/// A logged-in session for `user`.
fn login(db: &Arc<Database>, user: &str, pw: &str) -> Session {
    let mut s = Session::new_authenticating(db.clone());
    assert!(matches!(
        s.handle(hello()),
        Message::HelloAck { status: 0, .. }
    ));
    assert!(
        matches!(s.handle(auth(user, pw)), Message::AuthAck { status: 0, .. }),
        "login for {user} failed"
    );
    s
}

fn sql_ok(msg: Message) -> bool {
    matches!(msg, Message::SqlResult { status: 0, .. })
}

/// A permission-denied response: non-zero status with the authz error code.
fn denied(msg: Message) -> bool {
    match msg {
        Message::SqlResult { status, error, .. } => {
            status != 0 && error.is_some_and(|e| e.error_code == 0x0101)
        }
        Message::DocResult { status, error, .. } => {
            status != 0 && error.is_some_and(|e| e.error_code == 0x0101)
        }
        Message::KvResult { status, error, .. } => {
            status != 0 && error.is_some_and(|e| e.error_code == 0x0101)
        }
        Message::TxnAck { status, error, .. } => {
            status != 0 && error.is_some_and(|e| e.error_code == 0x0101)
        }
        _ => false,
    }
}

fn doc_insert(coll: &str) -> Message {
    let d = Document::from_fields([("k".to_string(), DocValue::Int64(1))]);
    Message::DocOp {
        collection: coll.into(),
        command: DocCommand::InsertOne(d.encode().unwrap()),
    }
}

fn doc_find(coll: &str) -> Message {
    Message::DocOp {
        collection: coll.into(),
        command: DocCommand::Find {
            query: DocQuery::All.to_bytes().unwrap(),
            options: vec![],
        },
    }
}

fn kv_put(ns: &str) -> Message {
    Message::KvOp {
        namespace: ns.into(),
        command: KvCommand::Put {
            key: b"k".to_vec(),
            value: b"v".to_vec(),
        },
    }
}

fn kv_get(ns: &str) -> Message {
    Message::KvOp {
        namespace: ns.into(),
        command: KvCommand::Get { key: b"k".to_vec() },
    }
}

#[test]
fn read_only_user_cannot_write() {
    let db = database();

    // Admin sets up a table and the accounts.
    let mut admin = login(&db, "admin", "admin");
    assert!(sql_ok(admin.handle(sql(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT)"
    ))));
    assert!(sql_ok(admin.handle(sql("INSERT INTO t VALUES (1,10)"))));
    assert!(sql_ok(admin.handle(sql(
        "CREATE USER reader WITH PASSWORD 'pw' ROLE readonly"
    ))));
    assert!(sql_ok(admin.handle(sql(
        "CREATE USER writer WITH PASSWORD 'pw' ROLE readwrite"
    ))));

    // Reader can SELECT but not mutate, and cannot run admin statements.
    let mut reader = login(&db, "reader", "pw");
    assert!(sql_ok(reader.handle(sql("SELECT v FROM t"))));
    assert!(
        denied(reader.handle(sql("INSERT INTO t VALUES (2,20)"))),
        "INSERT must be denied"
    );
    assert!(
        denied(reader.handle(sql("UPDATE t SET v = 0 WHERE id = 1"))),
        "UPDATE must be denied"
    );
    assert!(
        denied(reader.handle(sql("DELETE FROM t WHERE id = 1"))),
        "DELETE must be denied"
    );
    assert!(
        denied(reader.handle(sql("CREATE TABLE x (id BIGINT)"))),
        "DDL must be denied"
    );
    assert!(
        denied(reader.handle(sql("CREATE USER mallory WITH PASSWORD 'pw'"))),
        "admin statement must be denied"
    );
    // A read-write transaction is denied; a read-only one is allowed.
    assert!(denied(reader.handle(Message::Begin {
        mode: prism_protocol::TxnMode::ReadWrite
    })));
    assert!(matches!(
        reader.handle(Message::Begin {
            mode: prism_protocol::TxnMode::ReadOnly
        }),
        Message::TxnAck { status: 0, .. }
    ));
    reader.handle(Message::Abort);

    // Writer can mutate.
    let mut writer = login(&db, "writer", "pw");
    assert!(sql_ok(writer.handle(sql("INSERT INTO t VALUES (3,30)"))));
}

#[test]
fn grant_and_revoke_take_effect() {
    let db = database();
    let mut admin = login(&db, "admin", "admin");
    assert!(sql_ok(
        admin.handle(sql("CREATE TABLE t (id BIGINT PRIMARY KEY)"))
    ));
    assert!(sql_ok(
        admin.handle(sql("CREATE USER u WITH PASSWORD 'pw' ROLE readonly"))
    ));

    let mut u = login(&db, "u", "pw");
    assert!(
        denied(u.handle(sql("INSERT INTO t VALUES (1)"))),
        "readonly cannot insert"
    );

    // Promote to readwrite — the next op in the same session is allowed.
    assert!(sql_ok(admin.handle(sql("GRANT readwrite TO u"))));
    assert!(
        sql_ok(u.handle(sql("INSERT INTO t VALUES (1)"))),
        "after GRANT, insert allowed"
    );

    // Revoke everything — even reads are denied now.
    assert!(sql_ok(admin.handle(sql("REVOKE ALL FROM u"))));
    assert!(
        denied(u.handle(sql("SELECT id FROM t"))),
        "after REVOKE, read denied"
    );
}

#[test]
fn privileges_apply_to_documents_and_kv() {
    let db = database();
    let mut admin = login(&db, "admin", "admin");
    assert!(sql_ok(admin.handle(sql(
        "CREATE USER reader WITH PASSWORD 'pw' ROLE readonly"
    ))));

    let mut reader = login(&db, "reader", "pw");
    // Reads are fine.
    assert!(matches!(
        reader.handle(doc_find("c")),
        Message::DocResult { status: 0, .. }
    ));
    assert!(matches!(
        reader.handle(kv_get("n")),
        Message::KvResult { status: 0, .. }
    ));
    // Writes are denied.
    assert!(denied(reader.handle(doc_insert("c"))), "doc insert denied");
    assert!(denied(reader.handle(kv_put("n"))), "kv put denied");
}

#[test]
fn users_and_grants_persist_across_restart() {
    let tmp = prism_testkit::TempDir::new("authz-persist").unwrap();
    let path = tmp.path().to_path_buf();

    // Session 1: admin creates accounts, grants, then everything closes.
    {
        let db = Arc::new(Database::open(&path).unwrap());
        let mut admin = login(&db, "admin", "admin");
        assert!(sql_ok(
            admin.handle(sql("CREATE TABLE t (id BIGINT PRIMARY KEY)"))
        ));
        assert!(sql_ok(admin.handle(sql(
            "CREATE USER reader WITH PASSWORD 'pw' ROLE readonly"
        ))));
        assert!(sql_ok(admin.handle(sql(
            "CREATE USER writer WITH PASSWORD 'pw' ROLE readwrite"
        ))));
        // Promote reader to read-write; the grant must persist too.
        assert!(sql_ok(admin.handle(sql("GRANT readwrite TO reader"))));
        drop(admin);
        drop(db);
    }

    // Session 2: reopen the same directory — accounts and grants survived.
    {
        let db = Arc::new(Database::open(&path).unwrap());
        // reader persisted *with* the granted read-write privilege.
        let mut reader = login(&db, "reader", "pw");
        assert!(
            sql_ok(reader.handle(sql("INSERT INTO t VALUES (1)"))),
            "granted readwrite survived restart"
        );
        // writer persisted.
        let mut writer = login(&db, "writer", "pw");
        assert!(sql_ok(writer.handle(sql("INSERT INTO t VALUES (2)"))));

        // Drop writer; the tombstone must persist.
        let mut admin = login(&db, "admin", "admin");
        assert!(sql_ok(admin.handle(sql("DROP USER writer"))));
        drop(admin);
        drop(reader);
        drop(writer);
        drop(db);
    }

    // Session 3: the dropped account can no longer authenticate.
    let db = Arc::new(Database::open(&path).unwrap());
    let mut s = Session::new_authenticating(db.clone());
    assert!(matches!(
        s.handle(hello()),
        Message::HelloAck { status: 0, .. }
    ));
    assert!(
        matches!(s.handle(auth("writer", "pw")), Message::AuthAck { status, .. } if status != 0),
        "dropped user must not authenticate after restart"
    );
    // The admin account also survived (it was persisted on first open).
    let mut admin = login(&db, "admin", "admin");
    assert!(sql_ok(admin.handle(sql("SELECT id FROM t"))));
}

#[test]
fn embedded_session_is_a_superuser() {
    // The in-process Session::new (SYSTEM) bypasses authorization entirely.
    let db = database();
    let mut s = Session::new(db);
    assert!(sql_ok(
        s.handle(sql("CREATE TABLE t (id BIGINT PRIMARY KEY)"))
    ));
    assert!(sql_ok(s.handle(sql("INSERT INTO t VALUES (1)"))));
    assert!(sql_ok(s.handle(sql(
        "CREATE USER svc WITH PASSWORD 'pw' ROLE readwrite"
    ))));
}
