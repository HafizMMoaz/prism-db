//! `prism-sql` — the relational engine.
//!
//! Parses SQL and executes it over the unified record store, so relational data
//! shares MVCC, locking, recovery, and cross-model transactions with KV and
//! documents. See `docs/components/sql-engine.md`.
//!
//! **Scope (this slice):** `CREATE TABLE`, `INSERT … VALUES`,
//! `SELECT <cols|*> FROM t [WHERE <predicate>] [ORDER BY col [ASC|DESC], …]
//! [LIMIT n] [OFFSET n]`, `UPDATE t SET … [WHERE …]`, and `DELETE FROM t
//! [WHERE …]` over a sequential scan (with a primary-key index seek for
//! `SELECT … WHERE pk = …`), and aggregates `COUNT`/`SUM`/`MIN`/`MAX` with an
//! optional `GROUP BY`, for the types `BOOL`/`BIGINT`/`TEXT`. Deferred: joins,
//! `AVG` (needs a floating-point type), `HAVING`, `ORDER BY` over aggregate
//! output, arithmetic in `SET`, updating a primary-key column, the formal
//! bind/rewrite/plan IR. The current executor interprets the parsed AST
//! directly against the catalog; the full parse→bind→plan→execute pipeline is a
//! follow-up.

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
use prism_index::BTree;
use sqlparser::ast::{
    Assignment, AssignmentTarget, BinaryOperator, ColumnOption, DataType, Delete,
    DuplicateTreatment, Expr, FromTable, Function, FunctionArg, FunctionArgExpr, FunctionArguments,
    GroupByExpr, OrderByExpr, Query, SelectItem, SetExpr, Statement, TableFactor, TableObject,
    TableWithJoins, Value as SqlValue,
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
    /// An `UPDATE` modified `count` rows.
    Update {
        /// Rows updated.
        count: usize,
    },
    /// A `DELETE` removed `count` rows.
    Delete {
        /// Rows deleted.
        count: usize,
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
            Statement::Update {
                table,
                assignments,
                selection,
                ..
            } => self.exec_update(txn, table, assignments, selection),
            Statement::Delete(del) => self.exec_delete(txn, del),
            other => Err(SqlError::Unsupported(format!(
                "statement: {}",
                statement_kind(&other)
            ))),
        }
    }

    fn exec_create_table(&self, ct: sqlparser::ast::CreateTable) -> Result<Outcome> {
        let name = object_name(&ct.name);
        let mut columns = Vec::with_capacity(ct.columns.len());
        let mut primary_key = None;
        for (idx, col) in ct.columns.iter().enumerate() {
            let ty = map_data_type(&col.data_type)?;
            let nullable = !col.options.iter().any(|o| {
                matches!(
                    o.option,
                    ColumnOption::NotNull
                        | ColumnOption::Unique {
                            is_primary: true,
                            ..
                        }
                )
            });
            if col.options.iter().any(|o| {
                matches!(
                    o.option,
                    ColumnOption::Unique {
                        is_primary: true,
                        ..
                    }
                )
            }) {
                if primary_key.is_some() {
                    return Err(SqlError::Unsupported(
                        "only one PRIMARY KEY column is supported".into(),
                    ));
                }
                primary_key = Some(idx);
            }
            columns.push(Column {
                name: col.name.value.clone(),
                ty,
                nullable,
            });
        }

        // A PRIMARY KEY column gets a durable B+tree index (key -> row RID).
        let index_root = if primary_key.is_some() {
            let tree = BTree::create(self.store.buffer(), self.store.wal())?;
            Some(tree.root_page())
        } else {
            None
        };

        self.catalog
            .create_table(&name, columns, primary_key, index_root)?;
        Ok(Outcome::CreateTable)
    }

    /// Open the primary-key index tree for `table`, if it has one.
    fn pk_index(&self, table: &Table) -> Option<BTree> {
        match (table.primary_key, table.index_root) {
            (Some(_), Some(root)) => Some(BTree::open(
                self.store.buffer(),
                self.store.wal(),
                root,
                usize::MAX,
            )),
            _ => None,
        }
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
        let index = self.pk_index(&table);
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

            // Maintain the primary-key index, rejecting a duplicate that is
            // visible to this transaction (committed, or our own).
            let pk_key = match (&index, table.primary_key) {
                (Some(tree), Some(pk_col)) => {
                    let key = encode_index_key(&row[pk_col])?;
                    if let Some(existing) = tree.search(&key)? {
                        if self.store.read(txn, existing)?.is_some() {
                            return Err(SqlError::Constraint(format!(
                                "duplicate primary key in {}",
                                table.name
                            )));
                        }
                    }
                    Some(key)
                }
                _ => None,
            };

            let bytes = types::encode_row(&types, &row)?;
            let rid = self.store.insert(txn, table.heap, &bytes)?;
            if let (Some(tree), Some(key)) = (&index, pk_key) {
                tree.insert(&key, rid)?;
            }
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

        // Aggregate query: any aggregate in the projection, or a GROUP BY.
        let group_keys = parse_group_by(&select.group_by, &table)?;
        if !group_keys.is_empty() || projection_has_aggregate(&select.projection) {
            return self.exec_aggregate(
                txn,
                &select.projection,
                &select.selection,
                &table,
                group_keys,
                &query,
            );
        }

        // Resolve the projection to column indices.
        let projection: Vec<usize> = resolve_projection(&select.projection, &table)?;
        let columns: Vec<String> = projection
            .iter()
            .map(|&i| table.columns[i].name.clone())
            .collect();

        let types = table.types();

        // Index seek: `WHERE <pk> = <literal>` resolves to a single B+tree
        // lookup instead of a full scan. Only taken when there is no ORDER BY /
        // LIMIT / OFFSET to apply (a single row makes ordering moot, but a
        // LIMIT/OFFSET still has to be honored — let the scan path do that).
        let plain = query.order_by.is_none() && query.limit.is_none() && query.offset.is_none();
        if plain {
            if let (Some(tree), Some(key_value)) = (
                self.pk_index(&table),
                self.pk_equality_literal(&select.selection, &table)?,
            ) {
                let key = encode_index_key(&key_value)?;
                let mut rows = Vec::new();
                if let Some(rid) = tree.search(&key)? {
                    if let Some(payload) = self.store.read(txn, rid)? {
                        let full = types::decode_row(&types, &payload)?;
                        rows.push(projection.iter().map(|&i| full[i].clone()).collect());
                    }
                }
                return Ok(Outcome::Select { columns, rows });
            }
        }

        // Otherwise, a full sequential scan with the predicate applied per row.
        // Keep whole rows so an ORDER BY can sort on columns that are not in the
        // projection; project only after ordering and LIMIT/OFFSET.
        let mut full_rows: Vec<Vec<Value>> = Vec::new();
        for (_, payload) in self.store.scan(txn, table.heap)? {
            let full = types::decode_row(&types, &payload)?;
            if let Some(pred) = &select.selection {
                if !self.matches(pred, &full, &table)? {
                    continue;
                }
            }
            full_rows.push(full);
        }

        // ORDER BY <col> [ASC|DESC] [, …] — a stable sort by the key columns.
        if let Some(order_by) = &query.order_by {
            let keys = resolve_order_keys(&order_by.exprs, &table)?;
            full_rows.sort_by(|a, b| order_cmp(&keys, a, b));
        }

        // OFFSET then LIMIT, both non-negative integer literals.
        let offset = match &query.offset {
            Some(o) => count_literal(&o.value)?,
            None => 0,
        };
        let limit = match &query.limit {
            Some(e) => count_literal(e)?,
            None => usize::MAX,
        };
        let rows = full_rows
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|full| projection.iter().map(|&i| full[i].clone()).collect())
            .collect();
        Ok(Outcome::Select { columns, rows })
    }

    /// Execute an aggregate `SELECT`: a projection of group-key columns and/or
    /// aggregate calls (`COUNT`/`SUM`/`MIN`/`MAX`), with an optional `GROUP BY`.
    ///
    /// Rows passing the `WHERE` predicate are partitioned by the group-key
    /// tuple (one implicit group covering all rows when there is no `GROUP BY`,
    /// so `SELECT COUNT(*)` over an empty table still yields a single `0` row).
    /// Each group produces one output row. Groups are emitted in ascending
    /// group-key order for determinism; `LIMIT`/`OFFSET` then apply.
    fn exec_aggregate(
        &self,
        txn: &TxnHandle,
        projection: &[SelectItem],
        selection: &Option<Expr>,
        table: &Table,
        group_keys: Vec<usize>,
        query: &Query,
    ) -> Result<Outcome> {
        // ORDER BY over aggregate output is not supported yet (it would need to
        // reference computed columns); LIMIT/OFFSET still apply below.
        if query.order_by.is_some() {
            return Err(SqlError::Unsupported(
                "ORDER BY with aggregates or GROUP BY".into(),
            ));
        }

        // Resolve each projection item to either a group-key column or an
        // aggregate, along with its output column name.
        let mut outputs: Vec<(String, OutputCol)> = Vec::with_capacity(projection.len());
        for item in projection {
            let (expr, alias) = match item {
                SelectItem::UnnamedExpr(e) => (e, None),
                SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.value.clone())),
                other => {
                    return Err(SqlError::Unsupported(format!(
                        "aggregate projection item: {other:?}"
                    )));
                }
            };
            match expr {
                Expr::Function(f) => {
                    let agg = parse_aggregate(f, table)?;
                    let name = alias.unwrap_or_else(|| expr.to_string());
                    outputs.push((name, OutputCol::Aggregate(agg)));
                }
                Expr::Identifier(id) => {
                    let idx = table
                        .column_index(&id.value)
                        .ok_or_else(|| SqlError::NoSuchColumn(id.value.clone()))?;
                    if !group_keys.contains(&idx) {
                        return Err(SqlError::Unsupported(format!(
                            "column {} must appear in GROUP BY or an aggregate",
                            id.value
                        )));
                    }
                    let name = alias.unwrap_or_else(|| id.value.clone());
                    outputs.push((name, OutputCol::GroupKey(idx)));
                }
                other => {
                    return Err(SqlError::Unsupported(format!(
                        "aggregate projection expression: {other:?}"
                    )));
                }
            }
        }

        // Gather the rows passing the predicate.
        let types = table.types();
        let mut rows_in: Vec<Vec<Value>> = Vec::new();
        for (_, payload) in self.store.scan(txn, table.heap)? {
            let full = types::decode_row(&types, &payload)?;
            if let Some(pred) = selection {
                if !self.matches(pred, &full, table)? {
                    continue;
                }
            }
            rows_in.push(full);
        }

        // Partition into groups, preserving first-seen order then sorting by key.
        let mut groups: Vec<(Vec<Value>, Vec<usize>)> = Vec::new();
        if group_keys.is_empty() {
            groups.push((Vec::new(), (0..rows_in.len()).collect()));
        } else {
            for (i, row) in rows_in.iter().enumerate() {
                let key: Vec<Value> = group_keys.iter().map(|&k| row[k].clone()).collect();
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some(entry) => entry.1.push(i),
                    None => groups.push((key, vec![i])),
                }
            }
            groups.sort_by(|a, b| key_cmp(&a.0, &b.0));
        }

        // Compute one output row per group.
        let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(groups.len());
        for (key, members) in &groups {
            let mut cells = Vec::with_capacity(outputs.len());
            for (_, col) in &outputs {
                let value = match col {
                    OutputCol::GroupKey(idx) => {
                        let pos = group_keys.iter().position(|k| k == idx).expect("group key");
                        key[pos].clone()
                    }
                    OutputCol::Aggregate(agg) => agg.compute(members, &rows_in)?,
                };
                cells.push(value);
            }
            out_rows.push(cells);
        }

        // LIMIT / OFFSET over the grouped output.
        let offset = match &query.offset {
            Some(o) => count_literal(&o.value)?,
            None => 0,
        };
        let limit = match &query.limit {
            Some(e) => count_literal(e)?,
            None => usize::MAX,
        };
        let rows = out_rows.into_iter().skip(offset).take(limit).collect();
        let columns = outputs.into_iter().map(|(name, _)| name).collect();
        Ok(Outcome::Select { columns, rows })
    }

    /// Execute `UPDATE t SET col = expr, … [WHERE pred]`.
    ///
    /// A sequential scan applies the predicate per row; matching rows are
    /// re-encoded and written as a new MVCC version. The primary-key index is
    /// repointed to each new version (its key is unchanged — updating a primary
    /// key is rejected, since that would need re-keying and a fresh uniqueness
    /// check). Index seeks for `UPDATE … WHERE pk = …` are a later optimization.
    fn exec_update(
        &self,
        txn: &TxnHandle,
        table: TableWithJoins,
        assignments: Vec<Assignment>,
        selection: Option<Expr>,
    ) -> Result<Outcome> {
        if !table.joins.is_empty() {
            return Err(SqlError::Unsupported("UPDATE with joins".into()));
        }
        let TableFactor::Table { name, .. } = &table.relation else {
            return Err(SqlError::Unsupported(
                "UPDATE target must be a table name".into(),
            ));
        };
        let table = self.catalog.table(&object_name(name))?;

        // Resolve each `SET col = expr` to a (column index, value expr) pair.
        let mut sets: Vec<(usize, Expr)> = Vec::with_capacity(assignments.len());
        for a in assignments {
            let AssignmentTarget::ColumnName(col) = a.target else {
                return Err(SqlError::Unsupported(
                    "UPDATE SET target must be a single column".into(),
                ));
            };
            let col_name = object_name(&col);
            let idx = table
                .column_index(&col_name)
                .ok_or(SqlError::NoSuchColumn(col_name))?;
            if table.primary_key == Some(idx) {
                return Err(SqlError::Unsupported(
                    "cannot UPDATE a PRIMARY KEY column".into(),
                ));
            }
            sets.push((idx, a.value));
        }

        let types = table.types();
        let index = self.pk_index(&table);
        let mut count = 0;
        // scan() materializes the visible rows up front, so writing new
        // versions in this loop cannot disturb the iteration.
        for (rid, payload) in self.store.scan(txn, table.heap)? {
            let mut row = types::decode_row(&types, &payload)?;
            if let Some(pred) = &selection {
                if !self.matches(pred, &row, &table)? {
                    continue;
                }
            }
            // Evaluate every assignment against the *original* row, then apply,
            // so `SET a = b, b = a` swaps rather than chaining.
            let mut updates = Vec::with_capacity(sets.len());
            for (idx, expr) in &sets {
                let value = self.eval(expr, &row, &table)?;
                if !matches!(value, Value::Null) && !value.type_matches(table.columns[*idx].ty) {
                    return Err(SqlError::Type(format!(
                        "value for column {} has the wrong type",
                        table.columns[*idx].name
                    )));
                }
                updates.push((*idx, value));
            }
            for (idx, value) in updates {
                row[idx] = value;
            }
            // Enforce NOT NULL on the resulting row.
            for (col, value) in table.columns.iter().zip(&row) {
                if !col.nullable && matches!(value, Value::Null) {
                    return Err(SqlError::Type(format!("column {} is NOT NULL", col.name)));
                }
            }

            let bytes = types::encode_row(&types, &row)?;
            let new_rid = self.store.update(txn, rid, &bytes)?;
            // update() writes a new version at a new RecordId; repoint the
            // primary-key index so a later seek finds the live version.
            if let (Some(tree), Some(pk_col)) = (&index, table.primary_key) {
                let key = encode_index_key(&row[pk_col])?;
                tree.insert(&key, new_rid)?;
            }
            count += 1;
        }
        Ok(Outcome::Update { count })
    }

    /// Execute `DELETE FROM t [WHERE pred]`.
    ///
    /// A sequential scan applies the predicate and deletes each matching row's
    /// version. Primary-key index entries are intentionally left in place: MVCC
    /// hides the deleted version (a seek reads through to find it gone), and a
    /// later re-insert of the same key overwrites the stale entry.
    fn exec_delete(&self, txn: &TxnHandle, del: Delete) -> Result<Outcome> {
        let (FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables)) = &del.from;
        if tables.len() != 1 || !tables[0].joins.is_empty() {
            return Err(SqlError::Unsupported(
                "DELETE needs exactly one table, no joins".into(),
            ));
        }
        let TableFactor::Table { name, .. } = &tables[0].relation else {
            return Err(SqlError::Unsupported(
                "DELETE target must be a table name".into(),
            ));
        };
        let table = self.catalog.table(&object_name(name))?;
        let types = table.types();
        let mut count = 0;
        for (rid, payload) in self.store.scan(txn, table.heap)? {
            if let Some(pred) = &del.selection {
                let row = types::decode_row(&types, &payload)?;
                if !self.matches(pred, &row, &table)? {
                    continue;
                }
            }
            self.store.delete(txn, rid)?;
            count += 1;
        }
        Ok(Outcome::Delete { count })
    }

    /// If `selection` is exactly `<pk> = <literal>` (either operand order) on a
    /// table with a primary key whose type matches the literal, return that
    /// literal value for an index seek; otherwise `None` (fall back to a scan).
    fn pk_equality_literal(
        &self,
        selection: &Option<Expr>,
        table: &Table,
    ) -> Result<Option<Value>> {
        let Some(pk_col) = table.primary_key else {
            return Ok(None);
        };
        let Some(Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        }) = selection
        else {
            return Ok(None);
        };
        let pk_name = &table.columns[pk_col].name;
        let lit = match (left.as_ref(), right.as_ref()) {
            (Expr::Identifier(id), other) if &id.value == pk_name => literal(other).ok(),
            (other, Expr::Identifier(id)) if &id.value == pk_name => literal(other).ok(),
            _ => None,
        };
        // Only seek when the literal's type matches the key column.
        Ok(lit.filter(|v| v.type_matches(table.columns[pk_col].ty)))
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

/// Resolve `ORDER BY` items to `(column index, ascending)` pairs. Only simple
/// column references are supported (ascending by default; `DESC` flips it).
fn resolve_order_keys(items: &[OrderByExpr], table: &Table) -> Result<Vec<(usize, bool)>> {
    let mut keys = Vec::with_capacity(items.len());
    for item in items {
        let Expr::Identifier(ident) = &item.expr else {
            return Err(SqlError::Unsupported(format!(
                "ORDER BY expression: {:?}",
                item.expr
            )));
        };
        let idx = table
            .column_index(&ident.value)
            .ok_or_else(|| SqlError::NoSuchColumn(ident.value.clone()))?;
        // `asc: None` means the default, which is ascending.
        keys.push((idx, item.asc != Some(false)));
    }
    Ok(keys)
}

/// Order two rows by a list of sort keys. NULLs sort last under ascending order
/// (and so first under descending), matching the common SQL default.
fn order_cmp(keys: &[(usize, bool)], a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for &(idx, ascending) in keys {
        let ord = value_cmp(&a[idx], &b[idx]);
        let ord = if ascending { ord } else { ord.reverse() };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// A total order over values for sorting. NULL is treated as greater than any
/// non-NULL value; mismatched types compare equal (they cannot arise within one
/// typed column).
fn value_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Int64(x), Value::Int64(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// Evaluate a `LIMIT`/`OFFSET` expression to a non-negative row count.
fn count_literal(expr: &Expr) -> Result<usize> {
    match literal(expr)? {
        Value::Int64(n) if n >= 0 => Ok(n as usize),
        Value::Int64(n) => Err(SqlError::Type(format!("LIMIT/OFFSET must be >= 0: {n}"))),
        other => Err(SqlError::Type(format!(
            "LIMIT/OFFSET must be an integer: {other:?}"
        ))),
    }
}

/// A column in an aggregate query's output: either a `GROUP BY` key (carried
/// through) or a computed aggregate.
enum OutputCol {
    GroupKey(usize),
    Aggregate(Aggregate),
}

/// A supported aggregate function.
#[derive(Clone, Copy)]
enum AggFunc {
    Count,
    Sum,
    Min,
    Max,
}

/// An aggregate call: a function over an optional column (`COUNT(*)` has none).
struct Aggregate {
    func: AggFunc,
    col: Option<usize>,
}

impl Aggregate {
    /// Fold this aggregate over the `members` rows (indices into `rows`).
    fn compute(&self, members: &[usize], rows: &[Vec<Value>]) -> Result<Value> {
        use std::cmp::Ordering;
        match self.func {
            // COUNT(*) counts rows; COUNT(col) counts non-NULL values.
            AggFunc::Count => {
                let n = match self.col {
                    None => members.len(),
                    Some(c) => members
                        .iter()
                        .filter(|&&i| !matches!(rows[i][c], Value::Null))
                        .count(),
                };
                Ok(Value::Int64(n as i64))
            }
            // SUM over integers, skipping NULLs; an empty/all-NULL group is NULL.
            AggFunc::Sum => {
                let c = self.col.expect("SUM has a column");
                let mut sum: i64 = 0;
                let mut seen = false;
                for &i in members {
                    match &rows[i][c] {
                        Value::Null => {}
                        Value::Int64(n) => {
                            sum += n;
                            seen = true;
                        }
                        other => {
                            return Err(SqlError::Type(format!(
                                "SUM over a non-integer value: {other:?}"
                            )));
                        }
                    }
                }
                Ok(if seen { Value::Int64(sum) } else { Value::Null })
            }
            // MIN/MAX over any comparable type, skipping NULLs; empty group NULL.
            AggFunc::Min | AggFunc::Max => {
                let c = self.col.expect("MIN/MAX has a column");
                let want_min = matches!(self.func, AggFunc::Min);
                let mut best: Option<&Value> = None;
                for &i in members {
                    let v = &rows[i][c];
                    if matches!(v, Value::Null) {
                        continue;
                    }
                    best = Some(match best {
                        None => v,
                        Some(cur) => {
                            let ord = value_cmp(v, cur);
                            let take = if want_min {
                                ord == Ordering::Less
                            } else {
                                ord == Ordering::Greater
                            };
                            if take { v } else { cur }
                        }
                    });
                }
                Ok(best.cloned().unwrap_or(Value::Null))
            }
        }
    }
}

/// Whether any projection item is an aggregate function call.
fn projection_has_aggregate(items: &[SelectItem]) -> bool {
    items.iter().any(|item| {
        matches!(
            item,
            SelectItem::UnnamedExpr(Expr::Function(_))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Function(_),
                    ..
                }
        )
    })
}

/// Parse a `GROUP BY` clause to a list of distinct key column indices. Only
/// simple column references are supported.
fn parse_group_by(group_by: &GroupByExpr, table: &Table) -> Result<Vec<usize>> {
    match group_by {
        GroupByExpr::Expressions(exprs, modifiers) => {
            if !modifiers.is_empty() {
                return Err(SqlError::Unsupported("GROUP BY modifiers".into()));
            }
            let mut keys = Vec::with_capacity(exprs.len());
            for e in exprs {
                let Expr::Identifier(id) = e else {
                    return Err(SqlError::Unsupported(format!("GROUP BY expression: {e:?}")));
                };
                let idx = table
                    .column_index(&id.value)
                    .ok_or_else(|| SqlError::NoSuchColumn(id.value.clone()))?;
                if !keys.contains(&idx) {
                    keys.push(idx);
                }
            }
            Ok(keys)
        }
        GroupByExpr::All(_) => Err(SqlError::Unsupported("GROUP BY ALL".into())),
    }
}

/// Parse an aggregate function call (`COUNT`/`SUM`/`MIN`/`MAX`) to an
/// [`Aggregate`]. `AVG` is rejected for now (it needs a floating-point type).
fn parse_aggregate(f: &Function, table: &Table) -> Result<Aggregate> {
    if f.over.is_some() || f.filter.is_some() || !f.within_group.is_empty() {
        return Err(SqlError::Unsupported(
            "window / FILTER / WITHIN GROUP aggregates".into(),
        ));
    }
    let name = object_name(&f.name).to_ascii_uppercase();
    let FunctionArguments::List(list) = &f.args else {
        return Err(SqlError::Unsupported(format!(
            "{name} requires an argument"
        )));
    };
    if list.duplicate_treatment == Some(DuplicateTreatment::Distinct) {
        return Err(SqlError::Unsupported("DISTINCT aggregates".into()));
    }
    if list.args.len() != 1 {
        return Err(SqlError::Unsupported(format!(
            "{name} takes exactly one argument"
        )));
    }
    let FunctionArg::Unnamed(arg) = &list.args[0] else {
        return Err(SqlError::Unsupported("named aggregate argument".into()));
    };
    // Resolve a column-identifier argument to its index.
    let col_of = |arg: &FunctionArgExpr| -> Result<usize> {
        match arg {
            FunctionArgExpr::Expr(Expr::Identifier(id)) => table
                .column_index(&id.value)
                .ok_or_else(|| SqlError::NoSuchColumn(id.value.clone())),
            other => Err(SqlError::Unsupported(format!(
                "aggregate argument: {other:?}"
            ))),
        }
    };
    let func = match name.as_str() {
        "COUNT" => {
            let col = match arg {
                FunctionArgExpr::Wildcard => None,
                other => Some(col_of(other)?),
            };
            return Ok(Aggregate {
                func: AggFunc::Count,
                col,
            });
        }
        "SUM" => AggFunc::Sum,
        "MIN" => AggFunc::Min,
        "MAX" => AggFunc::Max,
        "AVG" => {
            return Err(SqlError::Unsupported(
                "AVG needs a floating-point column type (deferred)".into(),
            ));
        }
        other => {
            return Err(SqlError::Unsupported(format!("aggregate function {other}")));
        }
    };
    Ok(Aggregate {
        func,
        col: Some(col_of(arg)?),
    })
}

/// Compare two group-key tuples element-wise using [`value_cmp`].
fn key_cmp(a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = value_cmp(x, y);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Encode a value as an order-preserving B+tree index key. Integers use a
/// sign-flipped big-endian form so the byte order matches numeric order.
fn encode_index_key(value: &Value) -> Result<Vec<u8>> {
    Ok(match value {
        Value::Int64(n) => (*n as u64 ^ (1u64 << 63)).to_be_bytes().to_vec(),
        Value::Text(s) => s.as_bytes().to_vec(),
        Value::Bool(b) => vec![u8::from(*b)],
        Value::Null => return Err(SqlError::Constraint("primary key cannot be NULL".into())),
    })
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
        Statement::Drop { .. } => "DROP",
        Statement::CreateIndex(_) => "CREATE INDEX",
        Statement::AlterTable { .. } => "ALTER TABLE",
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

    #[test]
    fn primary_key_seek_and_uniqueness() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE u (id BIGINT PRIMARY KEY, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO u VALUES (1,'a'),(2,'b'),(3,'c')")
            .unwrap();

        // Equality on the PK seeks the index and returns exactly that row.
        let r = rows(
            env.engine
                .execute_autocommit("SELECT name FROM u WHERE id = 2")
                .unwrap(),
        );
        assert_eq!(r, vec![vec![Value::Text("b".into())]]);

        // A miss returns nothing.
        assert!(
            rows(
                env.engine
                    .execute_autocommit("SELECT name FROM u WHERE id = 99")
                    .unwrap()
            )
            .is_empty()
        );

        // A duplicate primary key is rejected.
        assert!(matches!(
            env.engine
                .execute_autocommit("INSERT INTO u VALUES (2,'dup')"),
            Err(SqlError::Constraint(_))
        ));

        // A duplicate within a single multi-row INSERT is also rejected (the
        // first row's write is visible to the same transaction).
        assert!(matches!(
            env.engine
                .execute_autocommit("INSERT INTO u VALUES (7,'x'),(7,'y')"),
            Err(SqlError::Constraint(_))
        ));
    }

    #[test]
    fn primary_key_index_matches_a_scan() {
        // The index seek must agree with a full scan over a non-indexed column.
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE u (id BIGINT PRIMARY KEY, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO u VALUES (10,'ten'),(20,'twenty')")
            .unwrap();

        // Seek by PK (index path).
        let by_pk = rows(
            env.engine
                .execute_autocommit("SELECT id, name FROM u WHERE id = 20")
                .unwrap(),
        );
        // Same row found by scanning a non-key column (scan path).
        let by_scan = rows(
            env.engine
                .execute_autocommit("SELECT id, name FROM u WHERE name = 'twenty'")
                .unwrap(),
        );
        assert_eq!(by_pk, by_scan);
        assert_eq!(
            by_pk,
            vec![vec![Value::Int64(20), Value::Text("twenty".into())]]
        );
    }

    fn affected(outcome: Outcome) -> usize {
        match outcome {
            Outcome::Update { count } | Outcome::Delete { count } | Outcome::Insert { count } => {
                count
            }
            other => panic!("expected a row count, got {other:?}"),
        }
    }

    #[test]
    fn update_with_where_changes_only_matching_rows() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')")
            .unwrap();

        assert_eq!(
            affected(
                env.engine
                    .execute_autocommit("UPDATE t SET name = 'X' WHERE id >= 2")
                    .unwrap()
            ),
            2
        );

        let r = rows(
            env.engine
                .execute_autocommit("SELECT id, name FROM t")
                .unwrap(),
        );
        assert_eq!(
            r,
            vec![
                vec![Value::Int64(1), Value::Text("a".into())],
                vec![Value::Int64(2), Value::Text("X".into())],
                vec![Value::Int64(3), Value::Text("X".into())],
            ]
        );
    }

    #[test]
    fn update_without_where_touches_every_row() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT, flag BOOL)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,false),(2,false)")
            .unwrap();
        assert_eq!(
            affected(
                env.engine
                    .execute_autocommit("UPDATE t SET flag = true")
                    .unwrap()
            ),
            2
        );
        let r = rows(env.engine.execute_autocommit("SELECT flag FROM t").unwrap());
        assert!(r.iter().all(|row| row[0] == Value::Bool(true)));
    }

    #[test]
    fn update_evaluates_assignments_against_the_original_row() {
        // `SET a = b, b = a` must swap, not chain through the new value of `a`.
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (a TEXT, b TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES ('left','right')")
            .unwrap();
        env.engine
            .execute_autocommit("UPDATE t SET a = b, b = a")
            .unwrap();
        let r = rows(env.engine.execute_autocommit("SELECT a, b FROM t").unwrap());
        assert_eq!(
            r,
            vec![vec![
                Value::Text("right".into()),
                Value::Text("left".into())
            ]]
        );
    }

    #[test]
    fn update_keeps_the_primary_key_index_consistent() {
        // After an UPDATE the PK seek (index path) must still find the row, with
        // its new column values — i.e. the index was repointed to the new
        // version, not left dangling at the old one.
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE u (id BIGINT PRIMARY KEY, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO u VALUES (1,'a'),(2,'b')")
            .unwrap();
        env.engine
            .execute_autocommit("UPDATE u SET name = 'updated' WHERE id = 2")
            .unwrap();

        let r = rows(
            env.engine
                .execute_autocommit("SELECT name FROM u WHERE id = 2")
                .unwrap(),
        );
        assert_eq!(r, vec![vec![Value::Text("updated".into())]]);
    }

    #[test]
    fn update_rejects_changing_a_primary_key_and_not_null() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE u (id BIGINT PRIMARY KEY, name TEXT NOT NULL)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO u VALUES (1,'a')")
            .unwrap();
        // Updating the primary-key column is not supported.
        assert!(matches!(
            env.engine.execute_autocommit("UPDATE u SET id = 5"),
            Err(SqlError::Unsupported(_))
        ));
        // A NOT NULL column cannot be set to NULL.
        assert!(matches!(
            env.engine.execute_autocommit("UPDATE u SET name = NULL"),
            Err(SqlError::Type(_))
        ));
    }

    #[test]
    fn delete_removes_matching_rows_and_allows_pk_reuse() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE u (id BIGINT PRIMARY KEY, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO u VALUES (1,'a'),(2,'b'),(3,'c')")
            .unwrap();

        assert_eq!(
            affected(
                env.engine
                    .execute_autocommit("DELETE FROM u WHERE id = 2")
                    .unwrap()
            ),
            1
        );

        // The deleted row is gone from both the scan and the PK seek.
        let r = rows(env.engine.execute_autocommit("SELECT id FROM u").unwrap());
        assert_eq!(r, vec![vec![Value::Int64(1)], vec![Value::Int64(3)]]);
        assert!(
            rows(
                env.engine
                    .execute_autocommit("SELECT id FROM u WHERE id = 2")
                    .unwrap()
            )
            .is_empty()
        );

        // The freed primary key can be inserted again.
        assert_eq!(
            affected(
                env.engine
                    .execute_autocommit("INSERT INTO u VALUES (2,'fresh')")
                    .unwrap()
            ),
            1
        );
        let r = rows(
            env.engine
                .execute_autocommit("SELECT name FROM u WHERE id = 2")
                .unwrap(),
        );
        assert_eq!(r, vec![vec![Value::Text("fresh".into())]]);
    }

    #[test]
    fn delete_without_where_empties_the_table() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1),(2),(3)")
            .unwrap();
        assert_eq!(
            affected(env.engine.execute_autocommit("DELETE FROM t").unwrap()),
            3
        );
        assert!(rows(env.engine.execute_autocommit("SELECT * FROM t").unwrap()).is_empty());
    }

    #[test]
    fn update_and_delete_are_atomic_within_a_txn() {
        // Both statements run in one transaction; an abort rolls back both.
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,'a'),(2,'b')")
            .unwrap();

        let txn = env.engine.txns.begin(TxnMode::ReadWrite);
        env.engine.execute(&txn, "UPDATE t SET name = 'z'").unwrap();
        env.engine
            .execute(&txn, "DELETE FROM t WHERE id = 1")
            .unwrap();
        txn.abort().unwrap();

        // After the abort the original two rows are intact and unchanged.
        let r = rows(
            env.engine
                .execute_autocommit("SELECT id, name FROM t")
                .unwrap(),
        );
        assert_eq!(
            r,
            vec![
                vec![Value::Int64(1), Value::Text("a".into())],
                vec![Value::Int64(2), Value::Text("b".into())],
            ]
        );
    }

    fn ints(outcome: Outcome) -> Vec<i64> {
        rows(outcome)
            .into_iter()
            .map(|r| match r[0] {
                Value::Int64(n) => n,
                ref other => panic!("expected an int, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn order_by_sorts_ascending_and_descending() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (2,'b'),(1,'a'),(3,'c')")
            .unwrap();

        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t ORDER BY id")
                    .unwrap()
            ),
            vec![1, 2, 3]
        );
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t ORDER BY id DESC")
                    .unwrap()
            ),
            vec![3, 2, 1]
        );
    }

    #[test]
    fn order_by_can_sort_on_a_non_projected_column() {
        // The sort key `name` is not in the projection, but ordering still works
        // because we sort the full rows before projecting.
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,'charlie'),(2,'alice'),(3,'bob')")
            .unwrap();
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t ORDER BY name")
                    .unwrap()
            ),
            vec![2, 3, 1] // alice, bob, charlie
        );
    }

    #[test]
    fn order_by_multiple_keys_and_nulls_sort_last() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (grp BIGINT, ord BIGINT)")
            .unwrap();
        // Two groups; one row has a NULL ordering value.
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,20),(1,NULL),(1,10),(2,5)")
            .unwrap();
        let r = rows(
            env.engine
                .execute_autocommit("SELECT grp, ord FROM t ORDER BY grp ASC, ord ASC")
                .unwrap(),
        );
        assert_eq!(
            r,
            vec![
                vec![Value::Int64(1), Value::Int64(10)],
                vec![Value::Int64(1), Value::Int64(20)],
                vec![Value::Int64(1), Value::Null], // NULL sorts last within grp 1
                vec![Value::Int64(2), Value::Int64(5)],
            ]
        );
    }

    #[test]
    fn limit_and_offset_apply_after_ordering() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (5),(3),(1),(4),(2)")
            .unwrap();

        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t ORDER BY id LIMIT 2")
                    .unwrap()
            ),
            vec![1, 2]
        );
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 1")
                    .unwrap()
            ),
            vec![2, 3]
        );
        // LIMIT past the end is clamped to what's available.
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t ORDER BY id DESC LIMIT 10")
                    .unwrap()
            ),
            vec![5, 4, 3, 2, 1]
        );
    }

    #[test]
    fn limit_with_where_filters_then_caps() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1),(2),(3),(4),(5)")
            .unwrap();
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t WHERE id > 2 ORDER BY id LIMIT 1")
                    .unwrap()
            ),
            vec![3]
        );
    }

    fn one_row(outcome: Outcome) -> Vec<Value> {
        let mut r = rows(outcome);
        assert_eq!(r.len(), 1, "expected exactly one row");
        r.pop().unwrap()
    }

    #[test]
    fn whole_table_aggregates() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT, score BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,10),(2,30),(3,NULL),(4,20)")
            .unwrap();

        let row = one_row(
            env.engine
                .execute_autocommit(
                    "SELECT COUNT(*), COUNT(score), SUM(score), MIN(score), MAX(score) FROM t",
                )
                .unwrap(),
        );
        assert_eq!(
            row,
            vec![
                Value::Int64(4),  // COUNT(*) — all rows
                Value::Int64(3),  // COUNT(score) — non-NULL only
                Value::Int64(60), // SUM skips NULL
                Value::Int64(10), // MIN skips NULL
                Value::Int64(30), // MAX skips NULL
            ]
        );
    }

    #[test]
    fn aggregate_column_names_and_aliases() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1),(2)")
            .unwrap();
        let out = env
            .engine
            .execute_autocommit("SELECT COUNT(*) AS n, MAX(id) FROM t")
            .unwrap();
        match out {
            Outcome::Select { columns, rows } => {
                assert_eq!(columns, vec!["n", "MAX(id)"]);
                assert_eq!(rows, vec![vec![Value::Int64(2), Value::Int64(2)]]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn count_star_over_empty_table_is_zero() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT)")
            .unwrap();
        // No rows: the implicit single group still yields one output row.
        assert_eq!(
            one_row(
                env.engine
                    .execute_autocommit("SELECT COUNT(*), SUM(id) FROM t")
                    .unwrap()
            ),
            vec![Value::Int64(0), Value::Null] // SUM of nothing is NULL
        );
    }

    #[test]
    fn aggregates_respect_the_where_clause() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1),(2),(3),(4),(5)")
            .unwrap();
        assert_eq!(
            one_row(
                env.engine
                    .execute_autocommit("SELECT COUNT(*), SUM(id) FROM t WHERE id > 3")
                    .unwrap()
            ),
            vec![Value::Int64(2), Value::Int64(9)] // ids 4,5
        );
    }

    #[test]
    fn group_by_buckets_rows_and_orders_by_key() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE sales (region TEXT, amount BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit(
                "INSERT INTO sales VALUES ('west',10),('east',5),('west',20),('east',7),('north',3)",
            )
            .unwrap();

        let r = rows(
            env.engine
                .execute_autocommit(
                    "SELECT region, COUNT(*), SUM(amount) FROM sales GROUP BY region",
                )
                .unwrap(),
        );
        // Groups emitted in ascending key order: east, north, west.
        assert_eq!(
            r,
            vec![
                vec![
                    Value::Text("east".into()),
                    Value::Int64(2),
                    Value::Int64(12)
                ],
                vec![
                    Value::Text("north".into()),
                    Value::Int64(1),
                    Value::Int64(3)
                ],
                vec![
                    Value::Text("west".into()),
                    Value::Int64(2),
                    Value::Int64(30)
                ],
            ]
        );
    }

    #[test]
    fn group_by_with_limit() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (g BIGINT, v BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (3,1),(1,1),(2,1),(1,1)")
            .unwrap();
        // Groups sorted by key (1,2,3); LIMIT 2 keeps the first two.
        let r = rows(
            env.engine
                .execute_autocommit("SELECT g, COUNT(*) FROM t GROUP BY g LIMIT 2")
                .unwrap(),
        );
        assert_eq!(
            r,
            vec![
                vec![Value::Int64(1), Value::Int64(2)],
                vec![Value::Int64(2), Value::Int64(1)],
            ]
        );
    }

    #[test]
    fn aggregate_query_rejects_a_bare_non_grouped_column() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (g BIGINT, v BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,10)")
            .unwrap();
        // `v` is neither grouped nor aggregated.
        assert!(matches!(
            env.engine
                .execute_autocommit("SELECT g, v, COUNT(*) FROM t GROUP BY g"),
            Err(SqlError::Unsupported(_))
        ));
        // AVG is explicitly deferred.
        assert!(matches!(
            env.engine.execute_autocommit("SELECT AVG(v) FROM t"),
            Err(SqlError::Unsupported(_))
        ));
    }
}
