//! Logical export/import: dump a database's structure and data to a portable
//! text format, and restore it into a (possibly empty) database.
//!
//! The dump is taken from a single read-only transaction, so it is a consistent
//! point-in-time snapshot across all three models. The format is line-oriented:
//!
//! - relational tables as replayable SQL (`CREATE TABLE …;` + `INSERT …;`),
//! - documents as `DOC <collection> <hex>` (hex of the tagged-binary document),
//! - key–value pairs as `KV <namespace> <hexKey> <hexValue>`.
//!
//! Import replays the directives into the target database and persists the
//! catalog. SQL is the human-readable structure+data surface; documents and KV
//! values are hex so the round-trip is exact and dependency-free. A JSON-
//! readable document/KV rendering is a follow-up.

use std::fmt::Write as _;

use prism_core::txn::TxnMode;
use prism_doc::{Document, Filter};
use prism_sql::{Outcome, Type, Value};

use crate::Database;
use crate::error::{Result, ServerError};

/// What an [`import`] applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportStats {
    /// `CREATE TABLE` statements applied.
    pub tables: usize,
    /// Rows inserted.
    pub rows: usize,
    /// Documents inserted.
    pub documents: usize,
    /// Key–value pairs written.
    pub kv_pairs: usize,
}

const HEADER: &str = "-- prismdb dump v1";

/// Export the whole database to a dump string (a consistent snapshot).
pub fn export_to_string(db: &Database) -> Result<String> {
    let mut out = String::new();
    writeln!(out, "{HEADER}").ok();

    let txns = db.txns();
    let txn = txns.begin(TxnMode::ReadOnly);
    let result = export_body(db, &txn, &mut out);
    // A read-only snapshot: always release it.
    let _ = txn.commit();
    result?;
    Ok(out)
}

fn export_body(db: &Database, txn: &prism_core::txn::TxnHandle, out: &mut String) -> Result<()> {
    // ---- relational tables ----
    let catalog = db.sql().catalog();
    for name in catalog.table_names() {
        let table = catalog.table(&name)?;
        writeln!(out, "\n-- table: {name}").ok();
        writeln!(out, "{}", render_create_table(&name, &table)).ok();

        let outcome = db.sql().execute(txn, &format!("SELECT * FROM {name}"))?;
        if let Outcome::Select { columns, rows } = outcome {
            for row in rows {
                writeln!(out, "{}", render_insert(&name, &columns, &row)?).ok();
            }
        }
    }

    // ---- document collections ----
    for name in db.collection_names() {
        writeln!(out, "\n-- collection: {name}").ok();
        let coll = db.collection(&name)?;
        for doc in coll.find(txn, &Filter::All)? {
            let bytes = doc.encode().map_err(ServerError::from)?;
            writeln!(out, "DOC {name} {}", to_hex(&bytes)).ok();
        }
    }

    // ---- key–value namespaces ----
    for name in db.kv_namespace_names() {
        writeln!(out, "\n-- namespace: {name}").ok();
        let ns = db.kv_namespace(&name)?;
        for (key, value) in ns.entries(txn)? {
            writeln!(out, "KV {name} {} {}", to_hex(&key), to_hex(&value)).ok();
        }
    }
    Ok(())
}

/// Restore a dump into `db`, returning what was applied. Everything runs in one
/// transaction; on error nothing is committed.
pub fn import(db: &Database, dump: &str) -> Result<ImportStats> {
    let mut stats = ImportStats::default();
    let txns = db.txns();
    let txn = txns.begin(TxnMode::ReadWrite);
    match apply_dump(db, &txn, dump, &mut stats) {
        Ok(()) => {
            txn.commit()?;
            db.persist_sql_tables()?;
            db.checkpoint()?;
            Ok(stats)
        }
        Err(e) => {
            let _ = txn.abort();
            Err(e)
        }
    }
}

fn apply_dump(
    db: &Database,
    txn: &prism_core::txn::TxnHandle,
    dump: &str,
    stats: &mut ImportStats,
) -> Result<()> {
    for stmt in statements(dump) {
        let stmt = stmt.trim();
        if stmt.is_empty() || stmt.starts_with("--") {
            continue;
        }
        if let Some(rest) = stmt.strip_prefix("DOC ") {
            let (coll, hex) = rest.split_once(' ').ok_or_else(|| bad("DOC directive"))?;
            let bytes = from_hex(hex.trim())?;
            let doc = Document::decode(&bytes).map_err(ServerError::from)?;
            db.collection(coll.trim())?.insert_one(txn, doc)?;
            stats.documents += 1;
        } else if let Some(rest) = stmt.strip_prefix("KV ") {
            let mut parts = rest.split_whitespace();
            let ns = parts.next().ok_or_else(|| bad("KV namespace"))?;
            let key = from_hex(parts.next().ok_or_else(|| bad("KV key"))?)?;
            let value = from_hex(parts.next().ok_or_else(|| bad("KV value"))?)?;
            db.kv_namespace(ns)?.put(txn, &key, &value)?;
            stats.kv_pairs += 1;
        } else {
            // A SQL statement (with or without a trailing ';').
            let sql = stmt.strip_suffix(';').unwrap_or(stmt);
            match db.sql().execute(txn, sql)? {
                Outcome::CreateTable => stats.tables += 1,
                Outcome::Insert { count } => stats.rows += count,
                _ => {}
            }
        }
    }
    Ok(())
}

fn bad(what: &str) -> ServerError {
    ServerError::Unsupported(format!("malformed dump: {what}"))
}

/// Split a dump into directives. `DOC`/`KV`/comment lines are one directive
/// each; a SQL statement runs until a `;` that is outside a single-quoted
/// string (so embedded newlines and `;` inside text are handled).
fn statements(dump: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for line in dump.lines() {
        let trimmed = line.trim();
        if buf.is_empty()
            && (trimmed.is_empty()
                || trimmed.starts_with("--")
                || trimmed.starts_with("DOC ")
                || trimmed.starts_with("KV "))
        {
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
            continue;
        }
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(line);
        // Complete when quotes balance and the text ends with ';'.
        if buf.trim_end().ends_with(';') && buf.matches('\'').count() % 2 == 0 {
            out.push(std::mem::take(&mut buf));
        }
    }
    if !buf.trim().is_empty() {
        out.push(buf);
    }
    out
}

fn render_create_table(name: &str, table: &prism_sql::Table) -> String {
    let cols: Vec<String> = table
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut s = format!("{} {}", c.name, sql_type(c.ty));
            if table.primary_key == Some(i) {
                s.push_str(" PRIMARY KEY");
            } else if !c.nullable {
                s.push_str(" NOT NULL");
            }
            s
        })
        .collect();
    format!("CREATE TABLE {name} ({});", cols.join(", "))
}

fn render_insert(name: &str, columns: &[String], row: &[Value]) -> Result<String> {
    let cols = columns.join(", ");
    let vals: Vec<String> = row.iter().map(sql_literal).collect();
    Ok(format!(
        "INSERT INTO {name} ({cols}) VALUES ({});",
        vals.join(", ")
    ))
}

fn sql_type(ty: Type) -> &'static str {
    match ty {
        Type::Int64 => "BIGINT",
        Type::Double => "DOUBLE",
        Type::Timestamp => "TIMESTAMP",
        Type::Text => "TEXT",
        Type::Bool => "BOOL",
    }
}

fn sql_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Int64(n) => n.to_string(),
        // `{:?}` keeps a decimal point (e.g. `2.0`) so it re-parses as a double.
        Value::Double(d) => format!("{d:?}"),
        // Raw epoch microseconds; an integer re-imported into a TIMESTAMP column
        // round-trips exactly (no lossy date string).
        Value::Timestamp(t) => t.to_string(),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
    }
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").ok();
    }
    s
}

fn from_hex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        return Err(bad("odd-length hex"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| bad("invalid hex")))
        .collect()
}
