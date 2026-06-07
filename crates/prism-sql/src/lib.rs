//! `prism-sql` — the relational engine.
//!
//! Parses SQL and executes it over the unified record store, so relational data
//! shares MVCC, locking, recovery, and cross-model transactions with KV and
//! documents. See `docs/components/sql-engine.md`.
//!
//! **Scope (this slice):** `CREATE TABLE`, `INSERT … VALUES`, and
//! `SELECT <cols|*> FROM t [WHERE <predicate>]` over a sequential scan, for the
//! types `BOOL`/`BIGINT`/`TEXT`. Deferred: joins, aggregates, `ORDER BY`,
//! index scans, the formal bind/rewrite/plan IR, and schema persistence. The
//! current executor interprets the parsed AST directly against the in-memory
//! catalog; the full parse→bind→plan→execute pipeline is a follow-up.

pub mod catalog;
pub mod error;
pub mod types;

pub use catalog::{Catalog, Column, Table};
pub use error::{Result, SqlError};
pub use types::{Type, Value};

use std::sync::Arc;

use prism_core::TxnManager;
use prism_core::store::RecordStore;
use prism_core::txn::{TxnHandle, TxnMode};
use sqlparser::ast::{
    BinaryOperator, ColumnOption, DataType, Expr, Query, SelectItem, SetExpr, Statement,
    TableFactor, TableObject, Value as SqlValue,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

/// The base heap id for relational tables (kept disjoint from other models).
const FIRST_TABLE_HEAP: u64 = 1000;

/// The result of executing one statement.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// A `CREATE TABLE` completed.
    CreateTable,
    /// An `INSERT` affected `count` rows.
    Insert {
        /// Rows inserted.
        count: usize,
    },
    /// A `SELECT` returned rows.
    Select {
        /// Output column names.
        columns: Vec<String>,
        /// Result rows, each aligned with `columns`.
        rows: Vec<Vec<Value>>,
    },
}

/// The relational engine: parses and executes SQL over the record store.
pub struct SqlEngine {
    store: Arc<RecordStore>,
    txns: Arc<TxnManager>,
    catalog: Catalog,
}

impl SqlEngine {
    /// Create an engine over the given record store and transaction manager.
    pub fn new(store: Arc<RecordStore>, txns: Arc<TxnManager>) -> Self {
        Self {
            store,
            txns,
            catalog: Catalog::new(FIRST_TABLE_HEAP),
        }
    }

    /// The catalog (for tests / inspection).
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Execute a statement in its own transaction (begin → run → commit, or
    /// abort on error). Convenient for one-shot statements and demos.
    pub fn execute_autocommit(&self, sql: &str) -> Result<Outcome> {
        let txn = self.txns.begin(TxnMode::ReadWrite);
        match self.execute(&txn, sql) {
            Ok(outcome) => {
                txn.commit()?;
                Ok(outcome)
            }
            Err(e) => {
                let _ = txn.abort();
                Err(e)
            }
        }
    }

    /// Parse and execute a single SQL statement within `txn`.
    pub fn execute(&self, txn: &TxnHandle, sql: &str) -> Result<Outcome> {
        let mut stmts = Parser::parse_sql(&GenericDialect {}, sql)
            .map_err(|e| SqlError::Parse(e.to_string()))?;
        if stmts.len() != 1 {
            return Err(SqlError::Unsupported(
                "exactly one statement per execute() call".into(),
            ));
        }
        match stmts.pop().unwrap() {
            Statement::CreateTable(ct) => self.exec_create_table(ct),
            Statement::Insert(ins) => self.exec_insert(txn, ins),
            Statement::Query(q) => self.exec_select(txn, *q),
            other => Err(SqlError::Unsupported(format!(
                "statement: {}",
                statement_kind(&other)
            ))),
        }
    }

    fn exec_create_table(&self, ct: sqlparser::ast::CreateTable) -> Result<Outcome> {
        let name = object_name(&ct.name);
        let mut columns = Vec::with_capacity(ct.columns.len());
        for col in &ct.columns {
            let ty = map_data_type(&col.data_type)?;
            let nullable = !col
                .options
                .iter()
                .any(|o| matches!(o.option, ColumnOption::NotNull));
            columns.push(Column {
                name: col.name.value.clone(),
                ty,
                nullable,
            });
        }
        self.catalog.create_table(&name, columns)?;
        Ok(Outcome::CreateTable)
    }

    fn exec_insert(&self, txn: &TxnHandle, ins: sqlparser::ast::Insert) -> Result<Outcome> {
        let table_name = match &ins.table {
            TableObject::TableName(name) => object_name(name),
            other => return Err(SqlError::Unsupported(format!("INSERT target: {other:?}"))),
        };
        let table = self.catalog.table(&table_name)?;

        // Map the optional explicit column list to row positions.
        let target: Vec<usize> = if ins.columns.is_empty() {
            (0..table.columns.len()).collect()
        } else {
            ins.columns
                .iter()
                .map(|c| {
                    table
                        .column_index(&c.value)
                        .ok_or_else(|| SqlError::NoSuchColumn(c.value.clone()))
                })
                .collect::<Result<_>>()?
        };

        let source = ins
            .source
            .as_ref()
            .ok_or_else(|| SqlError::Unsupported("INSERT without VALUES".into()))?;
        let SetExpr::Values(values) = source.body.as_ref() else {
            return Err(SqlError::Unsupported("INSERT source must be VALUES".into()));
        };

        let types = table.types();
        let mut count = 0;
        for row_exprs in &values.rows {
            if row_exprs.len() != target.len() {
                return Err(SqlError::Type(format!(
                    "INSERT has {} values for {} columns",
                    row_exprs.len(),
                    target.len()
                )));
            }
            // Default every column to NULL, then fill the targeted positions.
            let mut row = vec![Value::Null; table.columns.len()];
            for (expr, &pos) in row_exprs.iter().zip(&target) {
                row[pos] = literal(expr)?;
            }
            // Enforce NOT NULL.
            for (col, value) in table.columns.iter().zip(&row) {
                if !col.nullable && matches!(value, Value::Null) {
                    return Err(SqlError::Type(format!("column {} is NOT NULL", col.name)));
                }
            }
            let bytes = types::encode_row(&types, &row)?;
            self.store.insert(txn, table.heap, &bytes)?;
            count += 1;
        }
        Ok(Outcome::Insert { count })
    }

    fn exec_select(&self, txn: &TxnHandle, query: Query) -> Result<Outcome> {
        let SetExpr::Select(select) = query.body.as_ref() else {
            return Err(SqlError::Unsupported(
                "only simple SELECT is supported".into(),
            ));
        };
        if select.from.len() != 1 || !select.from[0].joins.is_empty() {
            return Err(SqlError::Unsupported(
                "SELECT needs exactly one table, no joins".into(),
            ));
        }
        let TableFactor::Table { name, .. } = &select.from[0].relation else {
            return Err(SqlError::Unsupported("FROM must be a table name".into()));
        };
        let table = self.catalog.table(&object_name(name))?;

        // Resolve the projection to column indices.
        let projection: Vec<usize> = resolve_projection(&select.projection, &table)?;
        let columns: Vec<String> = projection
            .iter()
            .map(|&i| table.columns[i].name.clone())
            .collect();

        let types = table.types();
        let mut rows = Vec::new();
        for (_, payload) in self.store.scan(txn, table.heap)? {
            let full = types::decode_row(&types, &payload)?;
            if let Some(pred) = &select.selection {
                if !self.matches(pred, &full, &table)? {
                    continue;
                }
            }
            rows.push(projection.iter().map(|&i| full[i].clone()).collect());
        }
        Ok(Outcome::Select { columns, rows })
    }

    /// Whether `row` satisfies the boolean predicate `expr`.
    fn matches(&self, expr: &Expr, row: &[Value], table: &Table) -> Result<bool> {
        Ok(matches!(self.eval(expr, row, table)?, Value::Bool(true)))
    }

    /// Evaluate `expr` against `row`.
    fn eval(&self, expr: &Expr, row: &[Value], table: &Table) -> Result<Value> {
        match expr {
            Expr::Nested(inner) => self.eval(inner, row, table),
            Expr::Identifier(ident) => {
                let idx = table
                    .column_index(&ident.value)
                    .ok_or_else(|| SqlError::NoSuchColumn(ident.value.clone()))?;
                Ok(row[idx].clone())
            }
            Expr::Value(_) | Expr::UnaryOp { .. } => literal(expr),
            Expr::BinaryOp { left, op, right } => {
                if matches!(op, BinaryOperator::And | BinaryOperator::Or) {
                    let l = self.matches(left, row, table)?;
                    let r = self.matches(right, row, table)?;
                    return Ok(Value::Bool(match op {
                        BinaryOperator::And => l && r,
                        _ => l || r,
                    }));
                }
                let l = self.eval(left, row, table)?;
                let r = self.eval(right, row, table)?;
                Ok(Value::Bool(compare(op, &l, &r)))
            }
            other => Err(SqlError::Unsupported(format!("expression: {other:?}"))),
        }
    }
}

/// Compare two values under a comparison operator. NULL operands yield `false`
/// (SQL's three-valued logic collapses unknown to "row excluded" here).
fn compare(op: &BinaryOperator, l: &Value, r: &Value) -> bool {
    use std::cmp::Ordering;
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return false;
    }
    let ord = match (l, r) {
        (Value::Int64(a), Value::Int64(b)) => a.cmp(b),
        (Value::Text(a), Value::Text(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        _ => return false, // type mismatch: never matches
    };
    match op {
        BinaryOperator::Eq => ord == Ordering::Equal,
        BinaryOperator::NotEq => ord != Ordering::Equal,
        BinaryOperator::Lt => ord == Ordering::Less,
        BinaryOperator::LtEq => ord != Ordering::Greater,
        BinaryOperator::Gt => ord == Ordering::Greater,
        BinaryOperator::GtEq => ord != Ordering::Less,
        _ => false,
    }
}

fn resolve_projection(items: &[SelectItem], table: &Table) -> Result<Vec<usize>> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard(_) => out.extend(0..table.columns.len()),
            SelectItem::UnnamedExpr(Expr::Identifier(ident)) => {
                let idx = table
                    .column_index(&ident.value)
                    .ok_or_else(|| SqlError::NoSuchColumn(ident.value.clone()))?;
                out.push(idx);
            }
            other => return Err(SqlError::Unsupported(format!("projection: {other:?}"))),
        }
    }
    Ok(out)
}

/// Convert a literal expression (possibly a unary minus on a number) to a value.
fn literal(expr: &Expr) -> Result<Value> {
    match expr {
        Expr::Value(v) => sql_value(v),
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Minus,
            expr,
        } => match literal(expr)? {
            Value::Int64(n) => Ok(Value::Int64(-n)),
            other => Err(SqlError::Type(format!("cannot negate {other:?}"))),
        },
        other => Err(SqlError::Unsupported(format!("literal: {other:?}"))),
    }
}

fn sql_value(v: &SqlValue) -> Result<Value> {
    match v {
        SqlValue::Number(n, _) => n
            .parse::<i64>()
            .map(Value::Int64)
            .map_err(|_| SqlError::Type(format!("not an integer: {n}"))),
        SqlValue::SingleQuotedString(s) | SqlValue::DoubleQuotedString(s) => {
            Ok(Value::Text(s.clone()))
        }
        SqlValue::Boolean(b) => Ok(Value::Bool(*b)),
        SqlValue::Null => Ok(Value::Null),
        other => Err(SqlError::Unsupported(format!("value: {other:?}"))),
    }
}

fn map_data_type(dt: &DataType) -> Result<Type> {
    match dt {
        DataType::Boolean | DataType::Bool => Ok(Type::Bool),
        DataType::TinyInt(_)
        | DataType::SmallInt(_)
        | DataType::Int(_)
        | DataType::Integer(_)
        | DataType::BigInt(_) => Ok(Type::Int64),
        DataType::Text
        | DataType::String(_)
        | DataType::Varchar(_)
        | DataType::Char(_)
        | DataType::CharVarying(_) => Ok(Type::Text),
        other => Err(SqlError::Unsupported(format!("column type: {other:?}"))),
    }
}

/// The simple name of an object (last identifier), unquoted.
fn object_name(name: &sqlparser::ast::ObjectName) -> String {
    name.to_string().trim_matches('"').to_string()
}

fn statement_kind(s: &Statement) -> &'static str {
    match s {
        Statement::Update { .. } => "UPDATE",
        Statement::Delete(_) => "DELETE",
        Statement::Drop { .. } => "DROP",
        _ => "this statement",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_buffer::{BufferPool, Config as BufConfig};
    use prism_storage::DiskManager;
    use prism_testkit::TempDir;
    use prism_wal::{Config as WalConfig, SyncMode, Wal};

    struct Env {
        engine: SqlEngine,
        _tmp: TempDir,
    }

    fn env() -> Env {
        let tmp = TempDir::new("sql").unwrap();
        let disk = Arc::new(DiskManager::open(&tmp.path().join("heap.db"), true).unwrap());
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
            Arc::new(BufferPool::new(disk, wal.clone(), BufConfig { frame_count: 32 }).unwrap());
        let txns = Arc::new(TxnManager::new(wal.clone()));
        let store = Arc::new(RecordStore::new(buffer, wal, txns.clone()));
        Env {
            engine: SqlEngine::new(store, txns),
            _tmp: tmp,
        }
    }

    fn rows(outcome: Outcome) -> Vec<Vec<Value>> {
        match outcome {
            Outcome::Select { rows, .. } => rows,
            other => panic!("expected rows, got {other:?}"),
        }
    }

    #[test]
    fn create_insert_select() {
        let env = env();
        assert_eq!(
            env.engine
                .execute_autocommit(
                    "CREATE TABLE users (id BIGINT NOT NULL, name TEXT, active BOOL)"
                )
                .unwrap(),
            Outcome::CreateTable
        );
        assert_eq!(
            env.engine
                .execute_autocommit(
                    "INSERT INTO users VALUES (1, 'alice', true), (2, 'bob', false)"
                )
                .unwrap(),
            Outcome::Insert { count: 2 }
        );

        let out = env
            .engine
            .execute_autocommit("SELECT id, name FROM users")
            .unwrap();
        match out {
            Outcome::Select { columns, rows } => {
                assert_eq!(columns, vec!["id", "name"]);
                assert_eq!(rows.len(), 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn select_star_and_where() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')")
            .unwrap();

        let r = rows(
            env.engine
                .execute_autocommit("SELECT * FROM t WHERE id > 1")
                .unwrap(),
        );
        assert_eq!(r.len(), 2);
        assert!(
            r.iter()
                .all(|row| matches!(row[0], Value::Int64(n) if n > 1))
        );

        let r = rows(
            env.engine
                .execute_autocommit("SELECT * FROM t WHERE name = 'b'")
                .unwrap(),
        );
        assert_eq!(r, vec![vec![Value::Int64(2), Value::Text("b".into())]]);

        let r = rows(
            env.engine
                .execute_autocommit("SELECT * FROM t WHERE id >= 2 AND name <> 'c'")
                .unwrap(),
        );
        assert_eq!(r, vec![vec![Value::Int64(2), Value::Text("b".into())]]);
    }

    #[test]
    fn insert_with_column_list_and_nulls() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT NOT NULL, name TEXT)")
            .unwrap();
        // Only id provided; name defaults to NULL.
        env.engine
            .execute_autocommit("INSERT INTO t (id) VALUES (7)")
            .unwrap();
        let r = rows(
            env.engine
                .execute_autocommit("SELECT name, id FROM t")
                .unwrap(),
        );
        assert_eq!(r, vec![vec![Value::Null, Value::Int64(7)]]);
    }

    #[test]
    fn errors_are_reported() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT)")
            .unwrap();
        assert!(matches!(
            env.engine.execute_autocommit("CREATE TABLE t (id BIGINT)"),
            Err(SqlError::TableExists(_))
        ));
        assert!(matches!(
            env.engine.execute_autocommit("SELECT * FROM nope"),
            Err(SqlError::NoSuchTable(_))
        ));
        assert!(matches!(
            env.engine
                .execute_autocommit("INSERT INTO t (id) VALUES ('not an int')"),
            Err(SqlError::Type(_))
        ));
        // NOT NULL violation.
        env.engine
            .execute_autocommit("CREATE TABLE u (id BIGINT NOT NULL)")
            .unwrap();
        assert!(matches!(
            env.engine
                .execute_autocommit("INSERT INTO u (id) VALUES (NULL)"),
            Err(SqlError::Type(_))
        ));
    }

    #[test]
    fn select_sees_snapshot_within_explicit_txn() {
        // Two statements in one transaction: the second SELECT sees the INSERT.
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT)")
            .unwrap();
        let txn = env.engine.txns.begin(TxnMode::ReadWrite);
        env.engine
            .execute(&txn, "INSERT INTO t VALUES (1),(2)")
            .unwrap();
        let out = env.engine.execute(&txn, "SELECT * FROM t").unwrap();
        assert_eq!(
            rows(out).len(),
            2,
            "uncommitted insert is visible to its own txn"
        );
        txn.commit().unwrap();
    }
}
