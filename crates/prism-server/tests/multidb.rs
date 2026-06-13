//! Multi-database server: CREATE/DROP/SHOW DATABASE + USE over a session bound
//! to an Instance, database isolation, and the privileges around them.

use std::sync::Arc;

use prism_protocol::{AuthMechanism, Message, PROTOCOL_VERSION, Value as WireValue};
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
