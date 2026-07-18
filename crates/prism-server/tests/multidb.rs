//! Multi-database server: CREATE/DROP/SHOW DATABASE + USE over a session bound
//! to an Instance, database isolation, and the privileges around them.

use std::sync::Arc;

use prism_protocol::{
    AuthMechanism, FEATURE_CONNECT_DB, Message, PROTOCOL_VERSION, Value as WireValue,
};
use prism_server::{Instance, Session};
use prism_testkit::TempDir;

fn instance() -> (Arc<Instance>, TempDir) {
    let tmp = TempDir::new("multidb").unwrap();
    let inst = Arc::new(Instance::open(tmp.path()).unwrap());
    (inst, tmp)
}

fn hello() -> Message {
    Message::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "t".into(),
        client_version: "0".into(),
        features: 0,
        database: String::new(),
    }
}

fn hello_db(db: &str) -> Message {
    Message::hello(PROTOCOL_VERSION, "t", "0", db)
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

fn login(inst: &Arc<Instance>, user: &str, pw: &str) -> Session {
    let mut s = Session::for_instance(inst.clone());
    assert!(matches!(
        s.handle(hello()),
        Message::HelloAck { status: 0, .. }
    ));
    assert!(
        matches!(s.handle(auth(user, pw)), Message::AuthAck { status: 0, .. }),
        "login {user} failed"
    );
    s
}

fn ok(msg: Message) -> bool {
    matches!(msg, Message::SqlResult { status: 0, .. })
}

fn errored(msg: Message) -> bool {
    matches!(msg, Message::SqlResult { status, .. } if status != 0)
}

/// The string cells of a single-row-per-record result (for `SHOW GRANTS`).
fn grant_rows(msg: Message) -> Vec<(String, String)> {
    match msg {
        Message::SqlResult {
            status: 0, rows, ..
        } => rows
            .iter()
            .map(|r| match (&r[0], &r[1]) {
                (Some(WireValue::Str(db)), Some(WireValue::Str(p))) => (db.clone(), p.clone()),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("expected SqlResult, got {other:?}"),
    }
}

#[test]
fn create_use_show_and_isolation() {
    let (inst, _tmp) = instance();
    let mut s = login(&inst, "admin", "admin");

    // No database selected yet: data statements fail.
    assert!(
        errored(s.handle(sql("CREATE TABLE t (id BIGINT PRIMARY KEY)"))),
        "data op with no USE must error"
    );

    // Create two databases and select one.
    assert!(ok(s.handle(sql("CREATE DATABASE app"))));
    assert!(ok(s.handle(sql("CREATE DATABASE analytics"))));
    assert!(ok(s.handle(sql("USE app"))));
    assert!(ok(s.handle(sql("CREATE TABLE t (id BIGINT PRIMARY KEY)"))));
    assert!(ok(s.handle(sql("INSERT INTO t VALUES (1),(2)"))));

    // SHOW DATABASES lists both (sorted), as a one-column result.
    match s.handle(sql("SHOW DATABASES")) {
        Message::SqlResult {
            status: 0,
            columns,
            rows,
            ..
        } => {
            assert_eq!(columns[0].name, "Database");
            let names: Vec<_> = rows
                .iter()
                .map(|r| match &r[0] {
                    Some(WireValue::Str(s)) => s.clone(),
                    other => panic!("{other:?}"),
                })
                .collect();
            assert_eq!(names, vec!["analytics", "app"]);
        }
        other => panic!("expected SqlResult, got {other:?}"),
    }

    // Switching databases isolates data: `t` exists only in `app`.
    assert!(ok(s.handle(sql("USE analytics"))));
    assert!(
        errored(s.handle(sql("SELECT id FROM t"))),
        "table from `app` must not be visible in `analytics`"
    );

    // Back in `app`, the rows are there.
    assert!(ok(s.handle(sql("USE app"))));
    match s.handle(sql("SELECT id FROM t ORDER BY id")) {
        Message::SqlResult {
            status: 0, rows, ..
        } => assert_eq!(rows.len(), 2),
        other => panic!("expected SqlResult, got {other:?}"),
    }
}

#[test]
fn database_management_requires_admin() {
    let (inst, _tmp) = instance();
    {
        let mut admin = login(&inst, "admin", "admin");
        assert!(ok(admin.handle(sql("CREATE DATABASE app"))));
        assert!(ok(admin.handle(sql(
            "CREATE USER reader WITH PASSWORD 'pw' ROLE readonly"
        ))));
    }

    let mut reader = login(&inst, "reader", "pw");
    // A read-only user can select a database and read, but not manage databases.
    assert!(ok(reader.handle(sql("USE app"))));
    match reader.handle(sql("CREATE DATABASE evil")) {
        Message::SqlResult { status, error, .. } => {
            assert_ne!(status, 0);
            assert_eq!(error.unwrap().error_code, 0x0101, "permission denied code");
        }
        other => panic!("expected SqlResult, got {other:?}"),
    }
}

#[test]
fn introspection_lists_tables_and_columns() {
    let (inst, _tmp) = instance();
    let mut s = login(&inst, "admin", "admin");
    assert!(ok(s.handle(sql("CREATE DATABASE app"))));
    assert!(ok(s.handle(sql("USE app"))));
    assert!(ok(s.handle(sql(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)"
    ))));
    assert!(ok(
        s.handle(sql("CREATE TABLE orders (id BIGINT PRIMARY KEY)"))
    ));

    // SHOW TABLES lists the database's tables (sorted).
    match s.handle(sql("SHOW TABLES")) {
        Message::SqlResult {
            status: 0,
            columns,
            rows,
            ..
        } => {
            assert_eq!(columns[0].name, "Tables");
            let names: Vec<_> = rows
                .iter()
                .map(|r| match &r[0] {
                    Some(WireValue::Str(s)) => s.clone(),
                    other => panic!("{other:?}"),
                })
                .collect();
            assert_eq!(names, vec!["orders", "users"]);
        }
        other => panic!("expected SqlResult, got {other:?}"),
    }

    // DESCRIBE reports each column's Field / Type / Key.
    match s.handle(sql("DESCRIBE users")) {
        Message::SqlResult {
            status: 0,
            columns,
            rows,
            ..
        } => {
            assert_eq!(columns[0].name, "Field");
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][0], Some(WireValue::Str("id".into())));
            assert_eq!(rows[0][1], Some(WireValue::Str("BIGINT".into())));
            assert_eq!(rows[0][3], Some(WireValue::Str("PRI".into())));
            assert_eq!(rows[1][0], Some(WireValue::Str("name".into())));
            assert_eq!(rows[1][1], Some(WireValue::Str("TEXT".into())));
            assert_eq!(rows[1][3], Some(WireValue::Str(String::new())));
        }
        other => panic!("expected SqlResult, got {other:?}"),
    }
}

#[test]
fn connect_time_database_binds_session_without_use() {
    let (inst, _tmp) = instance();
    // Create the database with an admin session first.
    {
        let mut admin = login(&inst, "admin", "admin");
        assert!(ok(admin.handle(sql("CREATE DATABASE app"))));
        assert!(ok(admin.handle(sql("USE app"))));
        assert!(ok(
            admin.handle(sql("CREATE TABLE t (id BIGINT PRIMARY KEY)"))
        ));
        assert!(ok(admin.handle(sql("INSERT INTO t VALUES (1),(2),(3)"))));
    }

    // A fresh session names the database in Hello; the server echoes the
    // negotiated feature bit and binds it once Auth succeeds.
    let mut s = Session::for_instance(inst.clone());
    match s.handle(hello_db("app")) {
        Message::HelloAck {
            status: 0,
            features,
            ..
        } => assert_eq!(features & FEATURE_CONNECT_DB, FEATURE_CONNECT_DB),
        other => panic!("expected HelloAck, got {other:?}"),
    }
    assert!(matches!(
        s.handle(auth("admin", "admin")),
        Message::AuthAck { status: 0, .. }
    ));

    // No `USE` was issued, yet the data op resolves against `app`.
    match s.handle(sql("SELECT id FROM t ORDER BY id")) {
        Message::SqlResult {
            status: 0, rows, ..
        } => assert_eq!(rows.len(), 3),
        other => panic!("expected SqlResult, got {other:?}"),
    }
}

#[test]
fn connect_time_unknown_database_fails_after_auth() {
    let (inst, _tmp) = instance();
    let mut s = Session::for_instance(inst.clone());
    // Hello succeeds (the name is not resolved yet)...
    assert!(matches!(
        s.handle(hello_db("ghost")),
        Message::HelloAck { status: 0, .. }
    ));
    // ...but the bind happens after credentials are verified, so a missing
    // database fails the handshake (status 3 = database_unavailable) and closes.
    match s.handle(auth("admin", "admin")) {
        Message::AuthAck { status, error, .. } => {
            assert_eq!(status, 3, "database_unavailable");
            assert!(error.is_some());
        }
        other => panic!("expected AuthAck, got {other:?}"),
    }
    assert!(s.is_closing(), "session must close after a failed bind");
}

#[test]
fn hello_without_connect_db_negotiates_no_features() {
    let (inst, _tmp) = instance();
    let mut s = Session::for_instance(inst.clone());
    match s.handle(hello()) {
        Message::HelloAck {
            status: 0,
            features,
            ..
        } => assert_eq!(features, 0, "no features requested, none honored"),
        other => panic!("expected HelloAck, got {other:?}"),
    }
}

#[test]
fn per_database_grant_scopes_data_access() {
    let (inst, _tmp) = instance();
    {
        let mut admin = login(&inst, "admin", "admin");
        assert!(ok(admin.handle(sql("CREATE DATABASE app"))));
        assert!(ok(admin.handle(sql("CREATE DATABASE secret"))));
        // alice has no global access; she is granted readwrite only on `app`.
        assert!(ok(
            admin.handle(sql("CREATE USER alice WITH PASSWORD 'pw' ROLE none"))
        ));
        assert!(ok(admin.handle(sql("GRANT readwrite ON app TO alice"))));
    }

    let mut alice = login(&inst, "alice", "pw");
    // She can use and write to `app`.
    assert!(ok(alice.handle(sql("USE app"))));
    assert!(ok(
        alice.handle(sql("CREATE TABLE t (id BIGINT PRIMARY KEY)"))
    ));
    assert!(ok(alice.handle(sql("INSERT INTO t VALUES (1)"))));
    // But `secret`, with no grant, is closed to her - even `USE` is denied.
    assert!(
        errored(alice.handle(sql("USE secret"))),
        "no grant on secret => USE denied"
    );
}

#[test]
fn per_database_deny_overrides_global_access() {
    let (inst, _tmp) = instance();
    {
        let mut admin = login(&inst, "admin", "admin");
        assert!(ok(admin.handle(sql("CREATE DATABASE app"))));
        assert!(ok(admin.handle(sql("CREATE DATABASE secret"))));
        // bob has global readwrite, but is denied on `secret` specifically.
        assert!(ok(admin.handle(sql(
            "CREATE USER bob WITH PASSWORD 'pw' ROLE readwrite"
        ))));
        assert!(ok(admin.handle(sql("REVOKE ALL ON secret FROM bob"))));
    }

    let mut bob = login(&inst, "bob", "pw");
    assert!(
        ok(bob.handle(sql("USE app"))),
        "global readwrite covers app"
    );
    assert!(
        errored(bob.handle(sql("USE secret"))),
        "explicit deny on secret beats global readwrite"
    );
}

#[test]
fn grant_on_unknown_database_is_rejected() {
    let (inst, _tmp) = instance();
    let mut admin = login(&inst, "admin", "admin");
    assert!(ok(
        admin.handle(sql("CREATE USER carol WITH PASSWORD 'pw' ROLE none"))
    ));
    assert!(
        errored(admin.handle(sql("GRANT readwrite ON ghost TO carol"))),
        "granting on a nonexistent database must error"
    );
}

#[test]
fn show_grants_lists_global_and_overrides() {
    let (inst, _tmp) = instance();
    let mut admin = login(&inst, "admin", "admin");
    assert!(ok(admin.handle(sql("CREATE DATABASE app"))));
    assert!(ok(admin.handle(sql("CREATE DATABASE secret"))));
    assert!(ok(admin.handle(sql(
        "CREATE USER dave WITH PASSWORD 'pw' ROLE readonly"
    ))));
    assert!(ok(admin.handle(sql("GRANT readwrite ON app TO dave"))));
    assert!(ok(admin.handle(sql("REVOKE ALL ON secret FROM dave"))));

    let rows = grant_rows(admin.handle(sql("SHOW GRANTS FOR dave")));
    assert_eq!(
        rows,
        vec![
            ("*".to_string(), "readonly".to_string()),
            ("app".to_string(), "readwrite".to_string()),
            ("secret".to_string(), "none".to_string()),
        ]
    );
}

#[test]
fn per_database_grants_persist_across_restart() {
    let tmp = TempDir::new("grant-persist").unwrap();
    {
        let inst = Arc::new(Instance::open(tmp.path()).unwrap());
        let mut admin = login(&inst, "admin", "admin");
        assert!(ok(admin.handle(sql("CREATE DATABASE app"))));
        assert!(ok(admin.handle(sql("CREATE DATABASE secret"))));
        assert!(ok(
            admin.handle(sql("CREATE USER erin WITH PASSWORD 'pw' ROLE none"))
        ));
        assert!(ok(admin.handle(sql("GRANT readwrite ON app TO erin"))));
    }
    // Reopen the instance from disk: erin's per-database grant survived.
    let inst = Arc::new(Instance::open(tmp.path()).unwrap());
    let mut erin = login(&inst, "erin", "pw");
    assert!(ok(erin.handle(sql("USE app"))), "grant on app persisted");
    assert!(
        errored(erin.handle(sql("USE secret"))),
        "no grant on secret persisted"
    );
}

#[test]
fn drop_database_removes_it() {
    let (inst, _tmp) = instance();
    let mut s = login(&inst, "admin", "admin");
    assert!(ok(s.handle(sql("CREATE DATABASE tmp"))));
    assert!(ok(s.handle(sql("USE tmp"))));
    assert!(ok(s.handle(sql("CREATE TABLE t (id BIGINT PRIMARY KEY)"))));
    assert!(ok(s.handle(sql("DROP DATABASE tmp"))));
    assert_eq!(inst.list_databases().unwrap(), Vec::<String>::new());
    // After dropping the selected database, a data op errors until USE again.
    assert!(errored(s.handle(sql("SELECT id FROM t"))));
}
