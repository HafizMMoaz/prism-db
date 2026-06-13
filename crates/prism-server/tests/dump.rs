//! Logical export/import round-trips structure and data across all three models.

use std::sync::Arc;

use prism_core::txn::TxnMode;
use prism_doc::{DocValue, Document, Filter};
use prism_server::{Database, export_to_string, import};
use prism_testkit::TempDir;

fn doc(fields: &[(&str, DocValue)]) -> Document {
    Document::from_fields(fields.iter().map(|(k, v)| (k.to_string(), v.clone())))
}

#[test]
fn export_import_round_trips_all_models() {
    let tmp_src = TempDir::new("dump-src").unwrap();
    let tmp_dst = TempDir::new("dump-dst").unwrap();

    // ---- Build a source database with data in every model. ----
    let dump = {
        let src = Arc::new(Database::open(tmp_src.path()).unwrap());
        src.sql()
            .execute_autocommit(
                "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT NOT NULL, active BOOL)",
            )
            .unwrap();
        // A row with an escaped apostrophe and one with a NULL.
        src.sql()
            .execute_autocommit(
                "INSERT INTO users VALUES (1,'alice',true),(2,'o''brien',false),(3,'dave',NULL)",
            )
            .unwrap();
        src.persist_sql_tables().unwrap();

        {
            let txns = src.txns();
            let t = txns.begin(TxnMode::ReadWrite);
            src.collection("people")
                .unwrap()
                .insert_one(
                    &t,
                    doc(&[
                        ("name", DocValue::Str("carol".into())),
                        ("age", DocValue::Int64(41)),
                    ]),
                )
                .unwrap();
            src.collection("people")
                .unwrap()
                .insert_one(&t, doc(&[("name", DocValue::Str("erin".into()))]))
                .unwrap();
            let ns = src.kv_namespace("sess").unwrap();
            ns.put(&t, b"k1", b"hello").unwrap();
            ns.put(&t, &[0u8, 1, 2, 255], &[9u8, 8, 7]).unwrap(); // binary key/value
            t.commit().unwrap();
        }

        export_to_string(&src).unwrap()
    };

    // The dump is readable SQL for structure + data.
    assert!(dump.contains("CREATE TABLE users"));
    assert!(dump.contains("PRIMARY KEY"));
    assert!(dump.contains("'o''brien'"), "apostrophe escaped in dump");

    // ---- Restore into a fresh database. ----
    let dst = Arc::new(Database::open(tmp_dst.path()).unwrap());
    let stats = import(&dst, &dump).unwrap();
    assert_eq!(stats.tables, 1);
    assert_eq!(stats.rows, 3);
    assert_eq!(stats.documents, 2);
    assert_eq!(stats.kv_pairs, 2);

    // ---- Verify the data survived the round trip. ----
    use prism_sql::{Outcome, Value};
    match dst
        .sql()
        .execute_autocommit("SELECT id, name, active FROM users ORDER BY id")
        .unwrap()
    {
        Outcome::Select { rows, .. } => {
            assert_eq!(rows.len(), 3);
            assert_eq!(
                rows[1][1],
                Value::Text("o'brien".into()),
                "escaped text restored"
            );
            assert_eq!(rows[2][2], Value::Null, "NULL restored");
            assert_eq!(rows[0][2], Value::Bool(true));
        }
        other => panic!("expected Select, got {other:?}"),
    }

    let txns = dst.txns();
    let t = txns.begin(TxnMode::ReadOnly);
    let people = dst
        .collection("people")
        .unwrap()
        .find(&t, &Filter::All)
        .unwrap();
    assert_eq!(people.len(), 2, "both documents restored");
    assert!(
        people
            .iter()
            .any(|d| d.get("name") == Some(&DocValue::Str("carol".into()))
                && d.get("age") == Some(&DocValue::Int64(41)))
    );

    let ns = dst.kv_namespace("sess").unwrap();
    assert_eq!(ns.get(&t, b"k1").unwrap().as_deref(), Some(&b"hello"[..]));
    assert_eq!(
        ns.get(&t, &[0u8, 1, 2, 255]).unwrap().as_deref(),
        Some(&[9u8, 8, 7][..]),
        "binary key/value restored"
    );
    t.commit().unwrap();
}

#[test]
fn imported_database_survives_restart() {
    // Import, close, reopen: the restored objects are durable (catalog persisted).
    let tmp_src = TempDir::new("dump-src2").unwrap();
    let tmp_dst = TempDir::new("dump-dst2").unwrap();

    let dump = {
        let src = Arc::new(Database::open(tmp_src.path()).unwrap());
        src.sql()
            .execute_autocommit("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT)")
            .unwrap();
        src.sql()
            .execute_autocommit("INSERT INTO t VALUES (1,100)")
            .unwrap();
        src.persist_sql_tables().unwrap();
        export_to_string(&src).unwrap()
    };

    {
        let dst = Arc::new(Database::open(tmp_dst.path()).unwrap());
        import(&dst, &dump).unwrap();
        drop(dst);
    }

    // Reopen the destination: the imported table and row are still there.
    let dst = Arc::new(Database::open(tmp_dst.path()).unwrap());
    use prism_sql::{Outcome, Value};
    match dst
        .sql()
        .execute_autocommit("SELECT v FROM t WHERE id = 1")
        .unwrap()
    {
        Outcome::Select { rows, .. } => assert_eq!(rows[0][0], Value::Int64(100)),
        other => panic!("expected Select, got {other:?}"),
    }
}
