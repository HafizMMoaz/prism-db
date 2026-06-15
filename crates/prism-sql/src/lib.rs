//! `prism-sql` — the relational engine.
//!
//! Parses SQL and executes it over the unified record store, so relational data
//! shares MVCC, locking, recovery, and cross-model transactions with KV and
//! documents. See `docs/components/sql-engine.md`.
//!
//! **Scope (this slice):** `CREATE TABLE`, `CREATE [UNIQUE] INDEX … ON t (c, …)`
//! / `DROP INDEX` (single- or multi-column secondary indexes, UNIQUE or
//! non-unique; UNIQUE enforces on `INSERT`/`UPDATE`, and an equality `WHERE` over
//! all of an index's columns seeks it), `INSERT … VALUES` and `INSERT … SELECT`,
//! `SELECT [DISTINCT] <exprs|*> FROM t [JOIN …] [WHERE <predicate>]
//! [ORDER BY col [ASC|DESC], …] [LIMIT n] [OFFSET n]`, `UPDATE t SET … [WHERE …]`,
//! and `DELETE FROM t [WHERE …]` over a sequential scan (with an index seek when
//! the `WHERE` pins the primary key or a UNIQUE-indexed column to a literal —
//! `SELECT … WHERE col = …` — or bounds the primary key with a range,
//! `> >= < <= BETWEEN`). Queries combine with `UNION`/`INTERSECT`/`EXCEPT`
//! (`ALL` or distinct). Aggregates are `COUNT`/`SUM`/`AVG`/
//! `MIN`/`MAX` with an optional `GROUP BY … [HAVING <predicate>]`, for the types
//! `BOOL`/`BIGINT`/`DOUBLE`/`TIMESTAMP`/`TEXT` (integers widen to doubles in
//! mixed arithmetic; `TIMESTAMP` is epoch microseconds and parses from
//! `'YYYY-MM-DD[ HH:MM:SS]'` strings; `CAST(x AS <type>)` converts between
//! scalars). Multi-table queries support `INNER` / `LEFT` / `RIGHT` /
//! `FULL OUTER` / `CROSS` joins (and comma-separated cartesian products and
//! self-joins via aliases) by nested loop, with `ON`, `USING (…)`, and `NATURAL`
//! constraints (`USING`/`NATURAL` coalesce the join columns), and `t.col`-
//! qualified column references throughout `SELECT`/`WHERE`/`ON`/`GROUP BY`/
//! `HAVING`/`ORDER BY`. Expressions support arithmetic (`+ - * / %`),
//! comparisons, `AND`/`OR`/`NOT`, `IS [NOT] NULL`, `[NOT] IN (…)`,
//! `[NOT] BETWEEN … AND …`, `[NOT] LIKE` (`%`/`_`), `CASE`, and scalar
//! functions: date/time (`NOW`, `CURDATE`, `YEAR`/`MONTH`/`DAY`/`HOUR`/`MINUTE`/
//! `SECOND`/`QUARTER`/`DAYOFWEEK`/`DAYOFYEAR`, `DATEDIFF`, `DATE_ADD`/`DATE_SUB`
//! with `INTERVAL n DAY|HOUR|…`, `UNIX_TIMESTAMP`/`FROM_UNIXTIME`, over Unix epoch
//! seconds), string (`UPPER`/`LOWER`/`LENGTH`/`SUBSTR`/`TRIM`/`CONCAT`/`REPLACE`/
//! `LEFT`/`RIGHT`/`REVERSE`/`REPEAT`/`SPACE`/`LPAD`/`RPAD`/`INSTR`/`LOCATE`/
//! `ASCII`), numeric (`ABS`/`MOD`/`ROUND`/`CEIL`/`FLOOR`/`POW`/`SQRT`/`EXP`/`LN`/
//! `LOG`/`LOG10`/`LOG2`/`SIGN`/`TRUNCATE`/`PI`/`GREATEST`/`LEAST`), and control
//! flow (`IF`/`IFNULL`/`NULLIF`/`COALESCE`) — usable in `WHERE`, `SET`,
//! the select list, and `HAVING` (and `ORDER BY` over aggregate output, by name,
//! 1-based ordinal, or expression text). Subqueries are supported: scalar
//! `(SELECT …)`, `x [NOT] IN (SELECT …)`, `[NOT] EXISTS (SELECT …)`, and derived
//! tables `FROM (SELECT …) AS alias`; uncorrelated ones run once up front, and
//! `WHERE` subqueries may be correlated (run per outer row). Deferred: correlated
//! subqueries outside `WHERE` (e.g. in the select list), updating a primary-key
//! column, join predicate pushdown / index nested-loop, the formal
//! bind/rewrite/plan IR. The current executor interprets the parsed
//! AST directly against the catalog; the full parse→bind→plan→execute pipeline
//! is a follow-up.

pub mod catalog;
pub mod error;
pub mod types;

pub use catalog::{Catalog, Column, IndexDef, Table};
pub use error::{Result, SqlError};
pub use types::{Type, Value};

use std::sync::Arc;

use prism_core::RecordId;
use prism_core::TxnManager;
use prism_core::store::RecordStore;
use prism_core::txn::{TxnHandle, TxnMode};
use prism_index::BTree;
use sqlparser::ast::{
    AlterTableOperation, Assignment, AssignmentTarget, BinaryOperator, CeilFloorKind, ColumnDef,
    ColumnOption, DataType, DateTimeField, Delete, Distinct, DuplicateTreatment, Expr, FromTable,
    Function, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr, JoinConstraint,
    JoinOperator, ObjectName, ObjectType, OrderByExpr, Query, Select, SelectItem, SetExpr,
    SetOperator, SetQuantifier, Statement, TableAlias, TableFactor, TableObject, TableWithJoins,
    UnaryOperator, Value as SqlValue,
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
    /// A `DROP TABLE` removed the named table.
    DropTable {
        /// The dropped table's name (for catalog tombstoning).
        name: String,
    },
    /// An `ALTER TABLE` changed a table's schema.
    AlterTable {
        /// The table's (possibly new) name.
        table: String,
        /// The previous name when the operation was `RENAME TO`; `None` for an
        /// in-place schema change. Lets the catalog tombstone the old name.
        renamed_from: Option<String>,
    },
    /// A `CREATE [UNIQUE] INDEX` added a secondary index to `table`.
    CreateIndex {
        /// The table the index was added to (for re-persisting its schema).
        table: String,
    },
    /// A `DROP INDEX` removed a secondary index from `table`.
    DropIndex {
        /// The table the index belonged to.
        table: String,
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
            Statement::CreateIndex(ci) => self.exec_create_index(txn, ci),
            Statement::Insert(ins) => self.exec_insert(txn, ins),
            Statement::Query(q) => self.exec_select(txn, *q),
            Statement::Update {
                table,
                assignments,
                selection,
                ..
            } => self.exec_update(txn, table, assignments, selection),
            Statement::Delete(del) => self.exec_delete(txn, del),
            Statement::Drop {
                object_type,
                if_exists,
                names,
                ..
            } => self.exec_drop(object_type, if_exists, names),
            Statement::AlterTable {
                name, operations, ..
            } => self.exec_alter(txn, name, operations),
            other => Err(SqlError::Unsupported(format!(
                "statement: {}",
                statement_kind(&other)
            ))),
        }
    }

    /// `DROP TABLE [IF EXISTS] <name>`. Other object types are unsupported. The
    /// table is removed from the catalog; its heap and index pages are abandoned
    /// (no in-database page reclamation yet) but become unreachable.
    fn exec_drop(
        &self,
        object_type: ObjectType,
        if_exists: bool,
        names: Vec<sqlparser::ast::ObjectName>,
    ) -> Result<Outcome> {
        if names.len() != 1 {
            return Err(SqlError::Unsupported(
                "DROP supports one object at a time".into(),
            ));
        }
        let name = object_name(&names[0]);
        match object_type {
            ObjectType::Table => match self.catalog.drop_table(&name) {
                Ok(()) => Ok(Outcome::DropTable { name }),
                // `IF EXISTS` makes a missing table a no-op (still reported so the
                // server can persist an idempotent tombstone).
                Err(SqlError::NoSuchTable(_)) if if_exists => Ok(Outcome::DropTable { name }),
                Err(e) => Err(e),
            },
            ObjectType::Index => match self.catalog.drop_index(&name) {
                Ok(table) => Ok(Outcome::DropIndex { table }),
                Err(_) if if_exists => Ok(Outcome::DropIndex {
                    table: String::new(),
                }),
                Err(e) => Err(e),
            },
            other => Err(SqlError::Unsupported(format!("DROP {other}"))),
        }
    }

    /// `CREATE [UNIQUE] INDEX <name> ON <table> (<column>)`. Builds a durable
    /// B+tree mapping the column's value to each row's id, checking that existing
    /// values are already unique. Only single-column UNIQUE indexes are supported.
    fn exec_create_index(
        &self,
        txn: &TxnHandle,
        ci: sqlparser::ast::CreateIndex,
    ) -> Result<Outcome> {
        if ci.columns.is_empty() {
            return Err(SqlError::Unsupported(
                "an index needs at least one column".into(),
            ));
        }
        let table_name = object_name(&ci.table_name);
        let table = self.catalog.table(&table_name)?;
        let columns: Vec<usize> = ci
            .columns
            .iter()
            .map(|c| resolve_col_expr(&table, &c.expr))
            .collect::<Result<_>>()?;
        let unique = ci.unique;
        let name = match &ci.name {
            Some(n) => object_name(n),
            None => {
                let cols = columns
                    .iter()
                    .map(|&c| table.columns[c].name.as_str())
                    .collect::<Vec<_>>()
                    .join("_");
                format!("{table_name}_{cols}_idx")
            }
        };

        // Build the tree from the live rows. A UNIQUE index keys on the composite
        // value (rejecting duplicates as it goes); a non-unique index appends the
        // row id so duplicates coexist. Rows with a NULL in any indexed column are
        // not indexed.
        let tree = BTree::create(self.store.buffer(), self.store.wal())?;
        let types = table.types();
        for (rid, payload) in self.store.scan(txn, table.heap)? {
            let row = types::decode_row(&types, &payload)?;
            let Some(key) = index_row_key(&row, &columns)? else {
                continue;
            };
            if unique {
                if tree.search(&key)?.is_some() {
                    return Err(SqlError::Constraint(format!(
                        "index {name} would have duplicate values; cannot create as UNIQUE"
                    )));
                }
                tree.insert(&key, rid)?;
            } else {
                tree.insert(&nonunique_key(&key, rid), rid)?;
            }
        }

        self.catalog.add_index(
            &table_name,
            IndexDef {
                name,
                columns,
                unique,
                root: tree.root_page(),
            },
        )?;
        Ok(Outcome::CreateIndex { table: table_name })
    }

    /// Open the B+trees of `table`'s secondary indexes, paired with their defs.
    fn secondary_indexes(&self, table: &Table) -> Vec<(IndexDef, BTree)> {
        table
            .indexes
            .iter()
            .map(|def| {
                let tree = BTree::open(self.store.buffer(), self.store.wal(), def.root, usize::MAX);
                (def.clone(), tree)
            })
            .collect()
    }

    /// `ALTER TABLE <name> <op>`: one operation per statement. ADD/DROP COLUMN
    /// rewrite every row (the relational payload is positional, so an existing
    /// row encoded under the old schema must be re-encoded under the new one);
    /// RENAME COLUMN / RENAME TO are metadata-only.
    ///
    /// Like the rest of DDL, this is not safe to run concurrently with other
    /// access to the table (no online schema change).
    fn exec_alter(
        &self,
        txn: &TxnHandle,
        name: ObjectName,
        operations: Vec<AlterTableOperation>,
    ) -> Result<Outcome> {
        let table = self.catalog.table(&object_name(&name))?;
        if operations.len() != 1 {
            return Err(SqlError::Unsupported(
                "one operation per ALTER TABLE".into(),
            ));
        }
        match operations.into_iter().next().expect("one operation") {
            AlterTableOperation::AddColumn { column_def, .. } => {
                self.alter_add_column(txn, &table, column_def)
            }
            AlterTableOperation::DropColumn {
                column_name,
                if_exists,
                ..
            } => self.alter_drop_column(txn, &table, &column_name.value, if_exists),
            AlterTableOperation::RenameColumn {
                old_column_name,
                new_column_name,
            } => self.alter_rename_column(&table, &old_column_name.value, &new_column_name.value),
            AlterTableOperation::RenameTable { table_name } => {
                let new = object_name(&table_name);
                self.catalog.rename_table(&table.name, &new)?;
                Ok(Outcome::AlterTable {
                    table: new,
                    renamed_from: Some(table.name),
                })
            }
            other => Err(SqlError::Unsupported(format!("ALTER TABLE {other}"))),
        }
    }

    /// `ALTER TABLE … ADD COLUMN`: append a column, defaulting existing rows to
    /// NULL. A `NOT NULL` column is rejected on a non-empty table (no default).
    fn alter_add_column(&self, txn: &TxnHandle, table: &Table, def: ColumnDef) -> Result<Outcome> {
        let col_name = def.name.value.clone();
        if table.column_index(&col_name).is_some() {
            return Err(SqlError::Constraint(format!(
                "column {col_name} already exists"
            )));
        }
        if def.options.iter().any(|o| {
            matches!(
                o.option,
                ColumnOption::Unique {
                    is_primary: true,
                    ..
                }
            )
        }) {
            return Err(SqlError::Unsupported(
                "ADD COLUMN cannot add a PRIMARY KEY".into(),
            ));
        }
        let ty = map_data_type(&def.data_type)?;
        let not_null = def
            .options
            .iter()
            .any(|o| matches!(o.option, ColumnOption::NotNull));

        let old_types = table.types();
        let mut new_columns = table.columns.clone();
        new_columns.push(Column {
            name: col_name.clone(),
            ty,
            nullable: !not_null,
        });
        let new_types: Vec<Type> = new_columns.iter().map(|c| c.ty).collect();

        let rows = self.store.scan(txn, table.heap)?;
        if not_null && !rows.is_empty() {
            return Err(SqlError::Constraint(format!(
                "ADD COLUMN {col_name} NOT NULL requires an empty table (no default)"
            )));
        }
        let index = self.pk_index(table);
        for (rid, payload) in rows {
            let mut row = types::decode_row(&old_types, &payload)?;
            row.push(Value::Null);
            let bytes = types::encode_row(&new_types, &row)?;
            let new_rid = self.store.update(txn, rid, &bytes)?;
            if let (Some(tree), Some(pk_col)) = (&index, table.primary_key) {
                tree.insert(&encode_index_key(&row[pk_col])?, new_rid)?;
            }
        }
        self.catalog
            .redefine_table(&table.name, new_columns, table.primary_key)?;
        Ok(Outcome::AlterTable {
            table: table.name.clone(),
            renamed_from: None,
        })
    }

    /// `ALTER TABLE … DROP COLUMN`: remove a column and re-encode every row. The
    /// PRIMARY KEY column and the last remaining column cannot be dropped.
    fn alter_drop_column(
        &self,
        txn: &TxnHandle,
        table: &Table,
        col: &str,
        if_exists: bool,
    ) -> Result<Outcome> {
        let idx = match table.column_index(col) {
            Some(i) => i,
            None if if_exists => {
                return Ok(Outcome::AlterTable {
                    table: table.name.clone(),
                    renamed_from: None,
                });
            }
            None => return Err(SqlError::NoSuchColumn(col.to_string())),
        };
        if table.primary_key == Some(idx) {
            return Err(SqlError::Unsupported(
                "cannot drop the PRIMARY KEY column".into(),
            ));
        }
        if table.columns.len() == 1 {
            return Err(SqlError::Unsupported("cannot drop the last column".into()));
        }

        let old_types = table.types();
        let mut new_columns = table.columns.clone();
        new_columns.remove(idx);
        let new_types: Vec<Type> = new_columns.iter().map(|c| c.ty).collect();
        // The PRIMARY KEY shifts left if it sat after the dropped column.
        let new_pk = table
            .primary_key
            .map(|pk| if pk > idx { pk - 1 } else { pk });

        let index = self.pk_index(table);
        for (rid, payload) in self.store.scan(txn, table.heap)? {
            let mut row = types::decode_row(&old_types, &payload)?;
            row.remove(idx);
            let bytes = types::encode_row(&new_types, &row)?;
            let new_rid = self.store.update(txn, rid, &bytes)?;
            if let (Some(tree), Some(pk_col)) = (&index, new_pk) {
                tree.insert(&encode_index_key(&row[pk_col])?, new_rid)?;
            }
        }
        self.catalog
            .redefine_table(&table.name, new_columns, new_pk)?;
        Ok(Outcome::AlterTable {
            table: table.name.clone(),
            renamed_from: None,
        })
    }

    /// `ALTER TABLE … RENAME COLUMN`: metadata only — the payload is positional,
    /// so no rows change.
    fn alter_rename_column(&self, table: &Table, from: &str, to: &str) -> Result<Outcome> {
        let idx = table
            .column_index(from)
            .ok_or_else(|| SqlError::NoSuchColumn(from.to_string()))?;
        if table.column_index(to).is_some() {
            return Err(SqlError::Constraint(format!("column {to} already exists")));
        }
        let mut new_columns = table.columns.clone();
        new_columns[idx].name = to.to_string();
        self.catalog
            .redefine_table(&table.name, new_columns, table.primary_key)?;
        Ok(Outcome::AlterTable {
            table: table.name.clone(),
            renamed_from: None,
        })
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

        // Build the full-width rows to insert (every column NULL-defaulted, the
        // targeted positions filled) from either a `VALUES` list or a query. An
        // `INSERT … SELECT` reads its source fully (the query materializes) before
        // any insert, so inserting from the same table is safe.
        let input_rows: Vec<Vec<Value>> = match source.body.as_ref() {
            SetExpr::Values(values) => {
                let mut rows = Vec::with_capacity(values.rows.len());
                for row_exprs in &values.rows {
                    if row_exprs.len() != target.len() {
                        return Err(SqlError::Type(format!(
                            "INSERT has {} values for {} columns",
                            row_exprs.len(),
                            target.len()
                        )));
                    }
                    let mut row = vec![Value::Null; table.columns.len()];
                    for (expr, &pos) in row_exprs.iter().zip(&target) {
                        row[pos] = literal(expr)?;
                    }
                    rows.push(row);
                }
                rows
            }
            _ => {
                let (columns, result) = match self.exec_select(txn, (**source).clone())? {
                    Outcome::Select { columns, rows } => (columns, rows),
                    other => {
                        return Err(SqlError::Unsupported(format!(
                            "INSERT source must be VALUES or a query, got {other:?}"
                        )));
                    }
                };
                if columns.len() != target.len() {
                    return Err(SqlError::Type(format!(
                        "INSERT source produces {} columns for {} target columns",
                        columns.len(),
                        target.len()
                    )));
                }
                result
                    .into_iter()
                    .map(|src| {
                        let mut row = vec![Value::Null; table.columns.len()];
                        for (value, &pos) in src.into_iter().zip(&target) {
                            row[pos] = value;
                        }
                        row
                    })
                    .collect()
            }
        };

        let types = table.types();
        let index = self.pk_index(&table);
        let sec = self.secondary_indexes(&table);
        let mut count = 0;
        for row in input_rows {
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

            // Secondary indexes: reject a visible duplicate for UNIQUE ones, then
            // collect the composite keys to insert once the row has a RID. A row
            // with a NULL in any indexed column is not indexed.
            let mut sec_keys: Vec<(usize, Vec<u8>)> = Vec::new();
            for (si, (def, tree)) in sec.iter().enumerate() {
                let Some(key) = index_row_key(&row, &def.columns)? else {
                    continue;
                };
                if def.unique {
                    if let Some(existing) = tree.search(&key)? {
                        if self.store.read(txn, existing)?.is_some() {
                            return Err(SqlError::Constraint(format!(
                                "duplicate value for UNIQUE index {}",
                                def.name
                            )));
                        }
                    }
                }
                sec_keys.push((si, key));
            }

            let bytes = types::encode_row(&types, &row)?;
            let rid = self.store.insert(txn, table.heap, &bytes)?;
            if let (Some(tree), Some(key)) = (&index, pk_key) {
                tree.insert(&key, rid)?;
            }
            for (si, key) in sec_keys {
                let (def, tree) = &sec[si];
                let stored = if def.unique {
                    key
                } else {
                    nonunique_key(&key, rid)
                };
                tree.insert(&stored, rid)?;
            }
            count += 1;
        }
        Ok(Outcome::Insert { count })
    }

    /// Execute a top-level query: a simple `SELECT`, a parenthesized query, or a
    /// set operation (`UNION`/`INTERSECT`/`EXCEPT`) combining two queries. A
    /// leading `WITH` (non-recursive CTEs) is expanded first by inlining.
    fn exec_select(&self, txn: &TxnHandle, mut query: Query) -> Result<Outcome> {
        if query.with.is_some() {
            expand_ctes(&mut query);
        }
        match query.body.as_ref() {
            SetExpr::Select(_) => self.exec_simple_select(txn, &query),
            SetExpr::Query(inner) => self.exec_select(txn, (**inner).clone()),
            SetExpr::SetOperation { .. } => self.exec_set_operation(txn, &query),
            other => Err(SqlError::Unsupported(format!("query body: {other:?}"))),
        }
    }

    /// Combine two queries with a set operator. Each side is evaluated without the
    /// outer `ORDER BY`/`LIMIT`/`OFFSET`; the two row sets are combined under
    /// multiset (`ALL`) or distinct semantics; the outer ordering and limit then
    /// apply to the result. Output column names come from the left side.
    fn exec_set_operation(&self, txn: &TxnHandle, query: &Query) -> Result<Outcome> {
        let SetExpr::SetOperation {
            op,
            set_quantifier,
            left,
            right,
        } = query.body.as_ref()
        else {
            unreachable!("exec_set_operation called on a non-set-operation body");
        };

        // Each side runs as its own query, stripped of the outer tail clauses.
        let side = |body: &SetExpr| -> Query {
            let mut q = query.clone();
            q.body = Box::new(body.clone());
            q.order_by = None;
            q.limit = None;
            q.offset = None;
            q
        };
        let (columns, lrows) = match self.exec_select(txn, side(left))? {
            Outcome::Select { columns, rows } => (columns, rows),
            _ => unreachable!("a SELECT side must produce rows"),
        };
        let (rcols, rrows) = match self.exec_select(txn, side(right))? {
            Outcome::Select { columns, rows } => (columns, rows),
            _ => unreachable!("a SELECT side must produce rows"),
        };
        if columns.len() != rcols.len() {
            return Err(SqlError::Type(format!(
                "set operation sides have {} and {} columns",
                columns.len(),
                rcols.len()
            )));
        }

        let all = matches!(
            set_quantifier,
            SetQuantifier::All | SetQuantifier::AllByName
        );
        let mut rows = match op {
            SetOperator::Union => set_union(lrows, rrows, all),
            SetOperator::Intersect => set_intersect(lrows, rrows, all),
            SetOperator::Except => set_except(lrows, rrows, all),
            other => return Err(SqlError::Unsupported(format!("set operator: {other:?}"))),
        };

        // ORDER BY over the combined output (output column by name, 1-based
        // ordinal, or expression text), then OFFSET / LIMIT.
        if let Some(order_by) = &query.order_by {
            let names: Vec<&str> = columns.iter().map(|s| s.as_str()).collect();
            let mut keys: Vec<(usize, bool)> = Vec::with_capacity(order_by.exprs.len());
            for item in &order_by.exprs {
                keys.push((
                    resolve_output_col(&item.expr, &names)?,
                    item.asc != Some(false),
                ));
            }
            rows.sort_by(|a, b| order_cmp(&keys, a, b));
        }
        let offset = match &query.offset {
            Some(o) => count_literal(&o.value)?,
            None => 0,
        };
        let limit = match &query.limit {
            Some(e) => count_literal(e)?,
            None => usize::MAX,
        };
        let rows = rows.into_iter().skip(offset).take(limit).collect();
        Ok(Outcome::Select { columns, rows })
    }

    /// Execute a non-set-operation `SELECT` (`query.body` is `SetExpr::Select`).
    fn exec_simple_select(&self, txn: &TxnHandle, query: &Query) -> Result<Outcome> {
        let select: &Select = match query.body.as_ref() {
            SetExpr::Select(s) => s.as_ref(),
            _ => unreachable!("exec_simple_select called on a non-SELECT body"),
        };

        // Resolve uncorrelated subqueries (scalar `(SELECT …)`, `IN (SELECT …)`,
        // `EXISTS (SELECT …)`) in WHERE/projection/HAVING by running them once and
        // substituting their results. Only clone the AST when one is present.
        let owned;
        let select = if select_has_subquery(select) {
            let mut s = select.clone();
            self.resolve_select_subqueries(txn, &mut s)?;
            owned = s;
            &owned
        } else {
            select
        };

        // SELECT DISTINCT dedupes the result rows (DISTINCT ON is not supported).
        let distinct = match &select.distinct {
            None => false,
            Some(Distinct::Distinct) => true,
            Some(other) => return Err(SqlError::Unsupported(format!("{other:?}"))),
        };

        // Any subquery still in WHERE after the up-front pass is correlated, and
        // is evaluated per row against the current row.
        let where_correlated = select.selection.as_ref().is_some_and(expr_has_subquery);

        // A single base table (no joins) keeps the primary-key index-seek fast
        // path; joins, several FROM items, or a derived table (subquery in FROM)
        // go through the nested-loop join materializer.
        if select.from.len() == 1
            && select.from[0].joins.is_empty()
            && matches!(select.from[0].relation, TableFactor::Table { .. })
        {
            let (qualifier, table) = self.relation_of(&select.from[0].relation)?;
            let schema = JoinSchema::single(&qualifier, &table);
            return self.select_single(
                txn,
                select,
                query,
                &table,
                &schema,
                distinct,
                where_correlated,
            );
        }

        let (schema, combined) = self.materialize_from(txn, &select.from)?;
        let mut filtered: Vec<Vec<Value>> = Vec::new();
        for row in combined {
            if self.matches_row(txn, &select.selection, &row, &schema, where_correlated)? {
                filtered.push(row);
            }
        }
        self.finish_select(select, query, &schema, filtered, distinct)
    }

    /// The single-base-table path: an index seek when the `WHERE` pins an
    /// indexed column to a literal, otherwise a full scan with the predicate
    /// applied per row. Produces the WHERE-filtered rows and hands off to
    /// [`Self::finish_select`].
    #[allow(clippy::too_many_arguments)]
    fn select_single(
        &self,
        txn: &TxnHandle,
        select: &Select,
        query: &Query,
        table: &Table,
        schema: &JoinSchema,
        distinct: bool,
        where_correlated: bool,
    ) -> Result<Outcome> {
        // Index seek (primary key or a UNIQUE secondary index) when applicable —
        // skipped when the predicate still holds a correlated subquery.
        if !where_correlated {
            if let Some(rows) = self.index_seek(txn, table, schema, &select.selection)? {
                return self.finish_select(select, query, schema, rows, distinct);
            }
        }
        // Otherwise a full sequential scan applying the predicate per row.
        let types = table.types();
        let mut filtered: Vec<Vec<Value>> = Vec::new();
        for (_, payload) in self.store.scan(txn, table.heap)? {
            let full = types::decode_row(&types, &payload)?;
            if self.matches_row(txn, &select.selection, &full, schema, where_correlated)? {
                filtered.push(full);
            }
        }
        self.finish_select(select, query, schema, filtered, distinct)
    }

    /// Seek an index instead of scanning when the `WHERE` clause (within a
    /// top-level `AND`) constrains an indexed column, returning the matching rows
    /// with the full predicate re-applied to each candidate (so residual
    /// conditions still filter). Tried in order: an equality on the primary key
    /// (point lookup) or on every column of a secondary index (UNIQUE → point,
    /// non-unique → the value's entries); then a primary-key **range** scan for
    /// `>`/`>=`/`<`/`<=`/`BETWEEN`. Returns `None` when no index applies, so the
    /// caller falls back to a scan.
    fn index_seek(
        &self,
        txn: &TxnHandle,
        table: &Table,
        schema: &JoinSchema,
        selection: &Option<Expr>,
    ) -> Result<Option<Vec<Vec<Value>>>> {
        let Some(pred) = selection else {
            return Ok(None);
        };
        let eqs = collect_equalities(pred);
        // A literal pinned to column `col`, if its type matches the column.
        let value_for = |col: usize| -> Option<Value> {
            eqs.iter()
                .find(|(n, _)| *n == table.columns[col].name)
                .map(|(_, v)| v.clone())
                .filter(|v| v.type_matches(table.columns[col].ty))
        };

        // The candidate row ids: first an equality seek (primary-key point lookup,
        // else a secondary index whose every column is pinned); failing that, a
        // primary-key range scan (`>`, `>=`, `<`, `<=`, `BETWEEN`).
        let equality = if let (Some(pk), Some(tree)) = (table.primary_key, self.pk_index(table)) {
            if let Some(v) = value_for(pk) {
                Some(tree.search(&encode_index_key(&v)?)?.into_iter().collect())
            } else {
                self.secondary_seek_rids(table, &value_for)?
            }
        } else {
            self.secondary_seek_rids(table, &value_for)?
        };
        let rids: Vec<RecordId> = match equality {
            Some(rids) => rids,
            None => match self.pk_range_seek_rids(table, pred)? {
                Some(rids) => rids,
                None => return Ok(None),
            },
        };

        let types = table.types();
        let mut out = Vec::new();
        for rid in rids {
            if let Some(payload) = self.store.read(txn, rid)? {
                let row = types::decode_row(&types, &payload)?;
                if self.matches(pred, &row, schema)? {
                    out.push(row);
                }
            }
        }
        Ok(Some(out))
    }

    /// Row ids from the first secondary index whose every column is pinned by
    /// `value_for`. UNIQUE → a point lookup; non-unique → a range scan of the
    /// value's `key ++ rid` entries. `None` means no secondary index applied.
    fn secondary_seek_rids(
        &self,
        table: &Table,
        value_for: &dyn Fn(usize) -> Option<Value>,
    ) -> Result<Option<Vec<RecordId>>> {
        'indexes: for def in &table.indexes {
            let mut values: Vec<Value> = Vec::with_capacity(def.columns.len());
            for &col in &def.columns {
                match value_for(col) {
                    Some(v) => values.push(v),
                    None => continue 'indexes,
                }
            }
            let key = frame_index_key(&values.iter().collect::<Vec<_>>())?;
            let tree = BTree::open(self.store.buffer(), self.store.wal(), def.root, usize::MAX);
            let rids = if def.unique {
                tree.search(&key)?.into_iter().collect()
            } else {
                let mut start = key.clone();
                start.extend_from_slice(&[0u8; RID_SUFFIX_LEN]);
                let mut end = key.clone();
                end.extend_from_slice(&[0xFFu8; RID_SUFFIX_LEN]);
                tree.range(&start, &end)?
                    .into_iter()
                    .filter(|(k, _)| {
                        k.len() == key.len() + RID_SUFFIX_LEN && k[..key.len()] == key[..]
                    })
                    .map(|(_, rid)| rid)
                    .collect()
            };
            return Ok(Some(rids));
        }
        Ok(None)
    }

    /// Row ids from a range scan of the primary-key index when `pred` bounds the
    /// PK column with `>`, `>=`, `<`, `<=`, or `BETWEEN` (within a top-level
    /// `AND`). Only fixed-width key types (numeric / timestamp / bool) are
    /// scanned — text keys vary in length, so an open-ended text range has no safe
    /// sentinel and falls back to a full scan. The bounds need only be a correct
    /// superset: `index_seek` re-applies the full predicate to every candidate.
    fn pk_range_seek_rids(&self, table: &Table, pred: &Expr) -> Result<Option<Vec<RecordId>>> {
        let (Some(pk), Some(tree)) = (table.primary_key, self.pk_index(table)) else {
            return Ok(None);
        };
        let ty = table.columns[pk].ty;
        let key_len = match ty {
            Type::Int64 | Type::Double | Type::Timestamp => 8,
            Type::Bool => 1,
            Type::Text => return Ok(None),
        };

        // Collect the tightest lower/upper bound on the PK column. Any valid
        // bound is correct (the predicate is re-checked); we keep the last seen.
        let pk_name = &table.columns[pk].name;
        let mut lower: Option<(Value, bool)> = None; // (value, inclusive)
        let mut upper: Option<(Value, bool)> = None;
        for (name, op, v) in collect_ranges(pred) {
            if name != *pk_name || !v.type_matches(ty) {
                continue;
            }
            match op {
                RangeOp::Gt => lower = Some((v, false)),
                RangeOp::Ge => lower = Some((v, true)),
                RangeOp::Lt => upper = Some((v, false)),
                RangeOp::Le => upper = Some((v, true)),
            }
        }
        if lower.is_none() && upper.is_none() {
            return Ok(None);
        }

        // `range(start, end)` is `[start, end)`. Appending a 0x00 byte to a key
        // makes it sort just after that key (so an exclusive lower / inclusive
        // upper steps past the bound value itself).
        let start = match &lower {
            None => Vec::new(),
            Some((v, true)) => encode_index_key(v)?,
            Some((v, false)) => {
                let mut k = encode_index_key(v)?;
                k.push(0x00);
                k
            }
        };
        let end = match &upper {
            None => vec![0xFFu8; key_len + 1],
            Some((v, false)) => encode_index_key(v)?,
            Some((v, true)) => {
                let mut k = encode_index_key(v)?;
                k.push(0x00);
                k
            }
        };
        let rids = tree
            .range(&start, &end)?
            .into_iter()
            .map(|(_, rid)| rid)
            .collect();
        Ok(Some(rids))
    }

    /// Shared tail of a `SELECT`: dispatch to the aggregate path when the query
    /// groups or aggregates, otherwise order / offset / limit / project the
    /// already-WHERE-filtered `rows`.
    fn finish_select(
        &self,
        select: &Select,
        query: &Query,
        schema: &JoinSchema,
        mut rows: Vec<Vec<Value>>,
        distinct: bool,
    ) -> Result<Outcome> {
        let group_keys = parse_group_by(&select.group_by, schema)?;
        if !group_keys.is_empty() || projection_has_aggregate(&select.projection) {
            return self.exec_aggregate(
                schema,
                &select.projection,
                &select.having,
                group_keys,
                rows,
                query,
                distinct,
            );
        }

        let projection = resolve_projection(&select.projection, schema)?;
        let columns: Vec<String> = projection.iter().map(|p| p.name.clone()).collect();

        if let Some(order_by) = &query.order_by {
            let keys = resolve_order_keys(&order_by.exprs, schema)?;
            rows.sort_by(|a, b| order_cmp(&keys, a, b));
        }

        let offset = match &query.offset {
            Some(o) => count_literal(&o.value)?,
            None => 0,
        };
        let limit = match &query.limit {
            Some(e) => count_literal(e)?,
            None => usize::MAX,
        };
        let mut out: Vec<Vec<Value>> = rows
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|full| self.project_row(&projection, &full, schema))
            .collect::<Result<_>>()?;
        if distinct {
            dedup_rows(&mut out);
        }
        Ok(Outcome::Select { columns, rows: out })
    }

    /// Resolve a `FROM`/`JOIN` table factor to `(qualifier, table)`. The
    /// qualifier is the alias if present, else the table name; it is what
    /// `t.col` references resolve against (so a self-join needs aliases).
    fn relation_of(&self, factor: &TableFactor) -> Result<(String, Table)> {
        match factor {
            TableFactor::Table { name, alias, .. } => {
                let table = self.catalog.table(&object_name(name))?;
                let qualifier = match alias {
                    Some(a) => a.name.value.clone(),
                    None => table.name.clone(),
                };
                Ok((qualifier, table))
            }
            other => Err(SqlError::Unsupported(format!("FROM item: {other:?}"))),
        }
    }

    /// Decode every row of `table` visible to `txn` (a full scan).
    fn scan_rows(&self, txn: &TxnHandle, table: &Table) -> Result<Vec<Vec<Value>>> {
        let types = table.types();
        let mut rows = Vec::new();
        for (_, payload) in self.store.scan(txn, table.heap)? {
            rows.push(types::decode_row(&types, &payload)?);
        }
        Ok(rows)
    }

    /// Materialize one `FROM`/`JOIN` source — a base table or a derived table
    /// (`(SELECT …) AS alias`) — into a schema and its rows.
    fn source_of(
        &self,
        txn: &TxnHandle,
        factor: &TableFactor,
    ) -> Result<(JoinSchema, Vec<Vec<Value>>)> {
        match factor {
            TableFactor::Table { .. } => {
                let (q, table) = self.relation_of(factor)?;
                let schema = JoinSchema::single(&q, &table);
                let rows = self.scan_rows(txn, &table)?;
                Ok((schema, rows))
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let alias = alias.as_ref().ok_or_else(|| {
                    SqlError::Unsupported("a derived table (subquery) needs an alias".into())
                })?;
                let q = alias.name.value.clone();
                let (columns, rows) = match self.exec_select(txn, (**subquery).clone())? {
                    Outcome::Select { columns, rows } => (columns, rows),
                    _ => {
                        return Err(SqlError::Unsupported(
                            "derived table must be a SELECT".into(),
                        ));
                    }
                };
                let cols = columns
                    .into_iter()
                    .map(|name| JoinCol {
                        qualifier: q.clone(),
                        name,
                    })
                    .collect();
                Ok((JoinSchema { cols }, rows))
            }
            other => Err(SqlError::Unsupported(format!("FROM item: {other:?}"))),
        }
    }

    /// Materialize the `FROM` clause (comma-separated tables cross-joined, each
    /// with its chain of `JOIN`s) into a combined schema and row set via
    /// nested-loop joins.
    fn materialize_from(
        &self,
        txn: &TxnHandle,
        from: &[TableWithJoins],
    ) -> Result<(JoinSchema, Vec<Vec<Value>>)> {
        let mut schema = JoinSchema { cols: Vec::new() };
        // The identity for a cross product is a single zero-width row.
        let mut rows: Vec<Vec<Value>> = vec![Vec::new()];

        for twj in from {
            let (base_schema, base_rows) = self.source_of(txn, &twj.relation)?;
            let (s, r) = cross_join(schema, rows, base_schema, base_rows);
            schema = s;
            rows = r;

            for join in &twj.joins {
                let (right_schema, right_rows) = self.source_of(txn, &join.relation)?;
                let (s, r) =
                    self.apply_join(schema, rows, right_schema, right_rows, &join.join_operator)?;
                schema = s;
                rows = r;
            }
        }
        Ok((schema, rows))
    }

    /// Rewrite the WHERE / projection / HAVING of `sel` with uncorrelated
    /// subqueries replaced by their computed values (run once each).
    fn resolve_select_subqueries(&self, txn: &TxnHandle, sel: &mut Select) -> Result<()> {
        if let Some(e) = sel.selection.take() {
            sel.selection = Some(self.resolve_subqueries(txn, e, None)?);
        }
        for item in &mut sel.projection {
            if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item {
                *e = self.resolve_subqueries(txn, e.clone(), None)?;
            }
        }
        if let Some(e) = sel.having.take() {
            sel.having = Some(self.resolve_subqueries(txn, e, None)?);
        }
        Ok(())
    }

    /// Evaluate the `WHERE` predicate for one row. When `correlated` is set the
    /// predicate still holds correlated subqueries; they are decorrelated against
    /// this row, run, and substituted before the predicate is evaluated.
    fn matches_row(
        &self,
        txn: &TxnHandle,
        selection: &Option<Expr>,
        row: &[Value],
        schema: &JoinSchema,
        correlated: bool,
    ) -> Result<bool> {
        match selection {
            None => Ok(true),
            Some(pred) if correlated => {
                let resolved = self.resolve_subqueries(txn, pred.clone(), Some((schema, row)))?;
                self.matches(&resolved, row, schema)
            }
            Some(pred) => self.matches(pred, row, schema),
        }
    }

    /// Whether a query references a column its own `FROM` does not provide — i.e.
    /// it is correlated with an enclosing query.
    fn query_is_correlated(&self, q: &Query) -> bool {
        let SetExpr::Select(s) = q.body.as_ref() else {
            return false;
        };
        let local = self.subquery_local_columns(q);
        let outside = |e: &Expr| {
            column_refs(e)
                .into_iter()
                .any(|(qual, name)| !is_local_column(&local, qual.as_deref(), &name))
        };
        if s.selection.as_ref().is_some_and(outside) || s.having.as_ref().is_some_and(outside) {
            return true;
        }
        for twj in &s.from {
            for join in &twj.joins {
                if let JoinOperator::Inner(JoinConstraint::On(e))
                | JoinOperator::LeftOuter(JoinConstraint::On(e))
                | JoinOperator::RightOuter(JoinConstraint::On(e))
                | JoinOperator::FullOuter(JoinConstraint::On(e)) = &join.join_operator
                {
                    if outside(e) {
                        return true;
                    }
                }
            }
        }
        s.projection.iter().any(|item| match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => outside(e),
            _ => false,
        })
    }

    /// The `(qualifier, column)` pairs a query's `FROM` base tables provide.
    /// Derived tables are skipped (their columns aren't known without running
    /// them), so a reference into one is conservatively treated as correlated.
    fn subquery_local_columns(&self, q: &Query) -> Vec<(String, String)> {
        let mut cols = Vec::new();
        if let SetExpr::Select(s) = q.body.as_ref() {
            for twj in &s.from {
                self.collect_factor_columns(&twj.relation, &mut cols);
                for join in &twj.joins {
                    self.collect_factor_columns(&join.relation, &mut cols);
                }
            }
        }
        cols
    }

    fn collect_factor_columns(&self, factor: &TableFactor, out: &mut Vec<(String, String)>) {
        match factor {
            TableFactor::Table { name, alias, .. } => {
                if let Ok(t) = self.catalog.table(&object_name(name)) {
                    let qualifier = match alias {
                        Some(a) => a.name.value.clone(),
                        None => t.name.clone(),
                    };
                    for c in &t.columns {
                        out.push((qualifier.clone(), c.name.clone()));
                    }
                }
            }
            // A derived table (incl. an inlined CTE) provides its projected output
            // columns under its alias; tracking them keeps a reference into a
            // derived table from being misread as a correlated outer reference.
            TableFactor::Derived {
                subquery,
                alias: Some(a),
                ..
            } => {
                let qualifier = a.name.value.clone();
                for name in self.derived_output_names(subquery) {
                    out.push((qualifier.clone(), name));
                }
            }
            _ => {}
        }
    }

    /// Best-effort output column names of a subquery used as a derived table —
    /// for correlation analysis only. Explicit projection items resolve to their
    /// name/alias; a `*` falls back to the inner `FROM`'s column names.
    fn derived_output_names(&self, q: &Query) -> Vec<String> {
        let body = match q.body.as_ref() {
            SetExpr::Select(s) => s,
            SetExpr::Query(inner) => return self.derived_output_names(inner),
            _ => return Vec::new(),
        };
        let mut names = Vec::new();
        for item in &body.projection {
            match item {
                SelectItem::UnnamedExpr(Expr::Identifier(id)) => names.push(id.value.clone()),
                SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) if !parts.is_empty() => {
                    names.push(parts.last().unwrap().value.clone());
                }
                SelectItem::ExprWithAlias { alias, .. } => names.push(alias.value.clone()),
                SelectItem::UnnamedExpr(e) => names.push(e.to_string()),
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => {
                    let mut inner = Vec::new();
                    for twj in &body.from {
                        self.collect_factor_columns(&twj.relation, &mut inner);
                        for join in &twj.joins {
                            self.collect_factor_columns(&join.relation, &mut inner);
                        }
                    }
                    names.extend(inner.into_iter().map(|(_, n)| n));
                }
            }
        }
        names
    }

    /// Rewrite a correlated subquery into an uncorrelated one for `(schema, row)`
    /// by replacing each reference to an outer column with that row's value.
    fn decorrelate(&self, mut q: Query, schema: &JoinSchema, row: &[Value]) -> Result<Query> {
        let local = self.subquery_local_columns(&q);
        if let SetExpr::Select(s) = q.body.as_mut() {
            let s = s.as_mut();
            if let Some(e) = s.selection.take() {
                s.selection = Some(self.decorrelate_expr(e, &local, schema, row)?);
            }
            if let Some(e) = s.having.take() {
                s.having = Some(self.decorrelate_expr(e, &local, schema, row)?);
            }
            for item in &mut s.projection {
                if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item
                {
                    *e = self.decorrelate_expr(e.clone(), &local, schema, row)?;
                }
            }
        }
        Ok(q)
    }

    /// Replace outer-column references (those not in `local`) in `expr` with the
    /// outer row's literal values. Does not descend into nested subqueries.
    fn decorrelate_expr(
        &self,
        expr: Expr,
        local: &[(String, String)],
        schema: &JoinSchema,
        row: &[Value],
    ) -> Result<Expr> {
        let rec = |this: &Self, e: Expr| this.decorrelate_expr(e, local, schema, row);
        Ok(match expr {
            Expr::Identifier(id) if !is_local_column(local, None, &id.value) => {
                match schema.resolve(None, &id.value) {
                    Ok(idx) => value_to_expr(row[idx].clone()),
                    Err(_) => Expr::Identifier(id),
                }
            }
            Expr::CompoundIdentifier(ref parts) if parts.len() == 2 => {
                let (q, n) = (&parts[0].value, &parts[1].value);
                if is_local_column(local, Some(q), n) {
                    expr
                } else {
                    match schema.resolve(Some(q), n) {
                        Ok(idx) => value_to_expr(row[idx].clone()),
                        Err(_) => expr,
                    }
                }
            }
            Expr::Nested(e) => Expr::Nested(Box::new(rec(self, *e)?)),
            Expr::IsNull(e) => Expr::IsNull(Box::new(rec(self, *e)?)),
            Expr::IsNotNull(e) => Expr::IsNotNull(Box::new(rec(self, *e)?)),
            Expr::UnaryOp { op, expr: e } => Expr::UnaryOp {
                op,
                expr: Box::new(rec(self, *e)?),
            },
            Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
                left: Box::new(rec(self, *left)?),
                op,
                right: Box::new(rec(self, *right)?),
            },
            Expr::Between {
                expr,
                negated,
                low,
                high,
            } => Expr::Between {
                expr: Box::new(rec(self, *expr)?),
                negated,
                low: Box::new(rec(self, *low)?),
                high: Box::new(rec(self, *high)?),
            },
            Expr::InList {
                expr,
                list,
                negated,
            } => Expr::InList {
                expr: Box::new(rec(self, *expr)?),
                list: list
                    .into_iter()
                    .map(|e| rec(self, e))
                    .collect::<Result<_>>()?,
                negated,
            },
            other => other,
        })
    }

    /// Replace `(SELECT …)` / `x IN (SELECT …)` / `EXISTS (SELECT …)` in `expr`
    /// with their results (uncorrelated only — the subquery runs standalone).
    /// `outer = None`: resolve uncorrelated subqueries up front (correlated ones
    /// are left in place for the per-row pass). `outer = Some((schema, row))`:
    /// the per-row pass — every subquery is decorrelated against the outer row
    /// and run.
    fn resolve_subqueries(
        &self,
        txn: &TxnHandle,
        expr: Expr,
        outer: Option<(&JoinSchema, &[Value])>,
    ) -> Result<Expr> {
        let rec = |this: &Self, e: Expr| this.resolve_subqueries(txn, e, outer);
        Ok(match expr {
            Expr::Subquery(q) => match outer {
                Some((os, r)) => {
                    let dq = self.decorrelate(*q, os, r)?;
                    value_to_expr(self.run_scalar_subquery(txn, dq)?)
                }
                None if self.query_is_correlated(&q) => Expr::Subquery(q),
                None => value_to_expr(self.run_scalar_subquery(txn, *q)?),
            },
            Expr::Exists { subquery, negated } => match outer {
                Some((os, r)) => {
                    let dq = self.decorrelate(*subquery, os, r)?;
                    let (_, rows) = self.run_subquery(txn, dq)?;
                    Expr::Value(SqlValue::Boolean(!rows.is_empty() ^ negated))
                }
                None if self.query_is_correlated(&subquery) => Expr::Exists { subquery, negated },
                None => {
                    let (_, rows) = self.run_subquery(txn, *subquery)?;
                    Expr::Value(SqlValue::Boolean(!rows.is_empty() ^ negated))
                }
            },
            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                let correlated_keep = outer.is_none() && self.query_is_correlated(&subquery);
                if correlated_keep {
                    Expr::InSubquery {
                        expr: Box::new(rec(self, *expr)?),
                        subquery,
                        negated,
                    }
                } else {
                    let q = match outer {
                        Some((os, r)) => self.decorrelate(*subquery, os, r)?,
                        None => *subquery,
                    };
                    Expr::InList {
                        expr: Box::new(rec(self, *expr)?),
                        list: self
                            .run_column_subquery(txn, q)?
                            .into_iter()
                            .map(value_to_expr)
                            .collect(),
                        negated,
                    }
                }
            }
            Expr::Nested(e) => Expr::Nested(Box::new(rec(self, *e)?)),
            Expr::IsNull(e) => Expr::IsNull(Box::new(rec(self, *e)?)),
            Expr::IsNotNull(e) => Expr::IsNotNull(Box::new(rec(self, *e)?)),
            Expr::UnaryOp { op, expr: e } => Expr::UnaryOp {
                op,
                expr: Box::new(rec(self, *e)?),
            },
            Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
                left: Box::new(rec(self, *left)?),
                op,
                right: Box::new(rec(self, *right)?),
            },
            Expr::Between {
                expr,
                negated,
                low,
                high,
            } => Expr::Between {
                expr: Box::new(rec(self, *expr)?),
                negated,
                low: Box::new(rec(self, *low)?),
                high: Box::new(rec(self, *high)?),
            },
            Expr::InList {
                expr,
                list,
                negated,
            } => Expr::InList {
                expr: Box::new(rec(self, *expr)?),
                list: list
                    .into_iter()
                    .map(|e| rec(self, e))
                    .collect::<Result<_>>()?,
                negated,
            },
            other => other,
        })
    }

    /// Run a subquery, returning its output columns and rows.
    fn run_subquery(&self, txn: &TxnHandle, q: Query) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
        match self.exec_select(txn, q)? {
            Outcome::Select { columns, rows } => Ok((columns, rows)),
            _ => Err(SqlError::Unsupported("subquery must be a SELECT".into())),
        }
    }

    /// Run a scalar subquery: exactly one column, at most one row (no row = NULL).
    fn run_scalar_subquery(&self, txn: &TxnHandle, q: Query) -> Result<Value> {
        let (columns, rows) = self.run_subquery(txn, q)?;
        if columns.len() != 1 {
            return Err(SqlError::Type(
                "a scalar subquery must return exactly one column".into(),
            ));
        }
        if rows.len() > 1 {
            return Err(SqlError::Type(
                "a scalar subquery returned more than one row".into(),
            ));
        }
        Ok(rows
            .into_iter()
            .next()
            .map_or(Value::Null, |mut r| r.remove(0)))
    }

    /// Run a single-column subquery, returning its column of values (for `IN`).
    fn run_column_subquery(&self, txn: &TxnHandle, q: Query) -> Result<Vec<Value>> {
        let (columns, rows) = self.run_subquery(txn, q)?;
        if columns.len() != 1 {
            return Err(SqlError::Type(
                "an IN subquery must return exactly one column".into(),
            ));
        }
        Ok(rows.into_iter().map(|mut r| r.remove(0)).collect())
    }

    /// Evaluate a join `ON` predicate against the concatenation of a left and a
    /// right row, resolved through the combined `schema`. `None` means an
    /// unconditional join (CROSS), always true.
    fn eval_on(
        &self,
        on: Option<&Expr>,
        schema: &JoinSchema,
        left: &[Value],
        right: &[Value],
    ) -> Result<bool> {
        match on {
            None => Ok(true),
            Some(e) => {
                let mut row = left.to_vec();
                row.extend_from_slice(right);
                self.matches(e, &row, schema)
            }
        }
    }

    /// Nested-loop join of accumulated `left` rows with a `right` source.
    /// Supports INNER, LEFT/RIGHT/FULL OUTER, and CROSS; the unmatched side of
    /// an outer join is padded with NULLs.
    fn apply_join(
        &self,
        left_schema: JoinSchema,
        left_rows: Vec<Vec<Value>>,
        right_schema: JoinSchema,
        right_rows: Vec<Vec<Value>>,
        op: &JoinOperator,
    ) -> Result<(JoinSchema, Vec<Vec<Value>>)> {
        let left_w = left_schema.cols.len();
        let right_w = right_schema.cols.len();

        enum Kind {
            Inner,
            Left,
            Right,
            Full,
            Cross,
        }
        let (kind, constraint): (Kind, Option<&JoinConstraint>) = match op {
            JoinOperator::Inner(c) => (Kind::Inner, Some(c)),
            JoinOperator::LeftOuter(c) => (Kind::Left, Some(c)),
            JoinOperator::RightOuter(c) => (Kind::Right, Some(c)),
            JoinOperator::FullOuter(c) => (Kind::Full, Some(c)),
            JoinOperator::CrossJoin => (Kind::Cross, None),
            other => return Err(SqlError::Unsupported(format!("join type: {other:?}"))),
        };

        // Resolve the join condition. USING/NATURAL become equi-pairs of
        // (left index, right index) and coalesce their columns afterward.
        let cond = match constraint {
            None | Some(JoinConstraint::None) => JoinCond::Always,
            Some(JoinConstraint::On(e)) => JoinCond::On(e),
            Some(JoinConstraint::Using(cols)) => {
                let mut pairs = Vec::with_capacity(cols.len());
                for name in cols {
                    let n = object_name(name);
                    let li = left_schema.resolve(None, &n).map_err(|_| {
                        SqlError::Unsupported(format!("USING column {n} is not in the left side"))
                    })?;
                    let ri = right_schema.resolve(None, &n).map_err(|_| {
                        SqlError::Unsupported(format!("USING column {n} is not in the right side"))
                    })?;
                    pairs.push((li, ri));
                }
                JoinCond::Pairs(pairs)
            }
            Some(JoinConstraint::Natural) => {
                let mut pairs = Vec::new();
                for (li, lc) in left_schema.cols.iter().enumerate() {
                    if let Ok(ri) = right_schema.resolve(None, &lc.name) {
                        pairs.push((li, ri));
                    }
                }
                JoinCond::Pairs(pairs)
            }
        };
        let coalesce_pairs = match &cond {
            JoinCond::Pairs(p) => Some(p.clone()),
            _ => None,
        };

        let mut combined = left_schema;
        combined.cols.extend(right_schema.cols);

        let cat = |l: &[Value], r: &[Value]| -> Vec<Value> {
            let mut row = Vec::with_capacity(l.len() + r.len());
            row.extend_from_slice(l);
            row.extend_from_slice(r);
            row
        };
        let null_left = || vec![Value::Null; left_w];
        let null_right = || vec![Value::Null; right_w];

        let mut out = Vec::new();
        match kind {
            Kind::Inner | Kind::Cross => {
                for l in &left_rows {
                    for r in &right_rows {
                        if self.join_match(&cond, &combined, l, r)? {
                            out.push(cat(l, r));
                        }
                    }
                }
            }
            Kind::Left => {
                for l in &left_rows {
                    let mut matched = false;
                    for r in &right_rows {
                        if self.join_match(&cond, &combined, l, r)? {
                            out.push(cat(l, r));
                            matched = true;
                        }
                    }
                    if !matched {
                        out.push(cat(l, &null_right()));
                    }
                }
            }
            Kind::Right => {
                for r in &right_rows {
                    let mut matched = false;
                    for l in &left_rows {
                        if self.join_match(&cond, &combined, l, r)? {
                            out.push(cat(l, r));
                            matched = true;
                        }
                    }
                    if !matched {
                        out.push(cat(&null_left(), r));
                    }
                }
            }
            Kind::Full => {
                let mut right_hit = vec![false; right_rows.len()];
                for l in &left_rows {
                    let mut matched = false;
                    for (ri, r) in right_rows.iter().enumerate() {
                        if self.join_match(&cond, &combined, l, r)? {
                            out.push(cat(l, r));
                            matched = true;
                            right_hit[ri] = true;
                        }
                    }
                    if !matched {
                        out.push(cat(l, &null_right()));
                    }
                }
                for (ri, r) in right_rows.iter().enumerate() {
                    if !right_hit[ri] {
                        out.push(cat(&null_left(), r));
                    }
                }
            }
        }

        // USING/NATURAL: emit each join column once (coalescing the two sides).
        if let Some(pairs) = coalesce_pairs {
            return Ok(coalesce_join(combined, out, &pairs, left_w));
        }
        Ok((combined, out))
    }

    /// Test a join condition against a left and a right row.
    fn join_match(
        &self,
        cond: &JoinCond,
        combined: &JoinSchema,
        l: &[Value],
        r: &[Value],
    ) -> Result<bool> {
        match cond {
            JoinCond::Always => Ok(true),
            JoinCond::On(e) => self.eval_on(Some(e), combined, l, r),
            JoinCond::Pairs(pairs) => {
                for &(li, ri) in pairs {
                    if !compare(&BinaryOperator::Eq, &l[li], &r[ri]) {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }

    /// Evaluate each projection item against `row`.
    fn project_row(
        &self,
        projection: &[ProjItem],
        row: &[Value],
        cols: &dyn ColumnResolver,
    ) -> Result<Vec<Value>> {
        projection
            .iter()
            .map(|p| match &p.kind {
                ProjKind::Col(i) => Ok(row[*i].clone()),
                ProjKind::Expr(e) => self.eval(e, row, cols),
            })
            .collect()
    }

    /// Execute an aggregate `SELECT`: a projection of group-key columns and/or
    /// aggregate calls (`COUNT`/`SUM`/`MIN`/`MAX`), with an optional `GROUP BY`.
    ///
    /// Rows passing the `WHERE` predicate are partitioned by the group-key
    /// tuple (one implicit group covering all rows when there is no `GROUP BY`,
    /// so `SELECT COUNT(*)` over an empty table still yields a single `0` row).
    /// Each group produces one output row. Groups are emitted in ascending
    /// group-key order for determinism; `LIMIT`/`OFFSET` then apply.
    #[allow(clippy::too_many_arguments)]
    fn exec_aggregate(
        &self,
        schema: &JoinSchema,
        projection: &[SelectItem],
        having: &Option<Expr>,
        group_keys: Vec<usize>,
        rows_in: Vec<Vec<Value>>,
        query: &Query,
        distinct: bool,
    ) -> Result<Outcome> {
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
                    let agg = parse_aggregate(f, schema)?;
                    let name = alias.unwrap_or_else(|| expr.to_string());
                    outputs.push((name, OutputCol::Aggregate(agg)));
                }
                Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
                    let idx = resolve_col_expr(schema, expr)?;
                    if !group_keys.contains(&idx) {
                        return Err(SqlError::Unsupported(format!(
                            "column {expr} must appear in GROUP BY or an aggregate"
                        )));
                    }
                    let name = alias.unwrap_or_else(|| expr.to_string());
                    outputs.push((name, OutputCol::GroupKey(idx)));
                }
                other => {
                    return Err(SqlError::Unsupported(format!(
                        "aggregate projection expression: {other:?}"
                    )));
                }
            }
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

        // Compute one output row per group, applying HAVING (a predicate that
        // may reference aggregates and group keys) to drop groups.
        let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(groups.len());
        for (key, members) in &groups {
            if let Some(pred) = having {
                let keep = Self::eval_having(pred, key, members, &rows_in, &group_keys, schema)?;
                if !matches!(keep, Value::Bool(true)) {
                    continue;
                }
            }
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

        // ORDER BY over the computed output: a key is an output column by name,
        // by 1-based ordinal (`ORDER BY 2`), or by its expression text
        // (`ORDER BY COUNT(*)`).
        if let Some(order_by) = &query.order_by {
            let names: Vec<&str> = outputs.iter().map(|(n, _)| n.as_str()).collect();
            let mut keys: Vec<(usize, bool)> = Vec::with_capacity(order_by.exprs.len());
            for item in &order_by.exprs {
                keys.push((
                    resolve_output_col(&item.expr, &names)?,
                    item.asc != Some(false),
                ));
            }
            out_rows.sort_by(|a, b| order_cmp(&keys, a, b));
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
        let mut rows: Vec<Vec<Value>> = out_rows.into_iter().skip(offset).take(limit).collect();
        if distinct {
            dedup_rows(&mut rows);
        }
        let columns = outputs.into_iter().map(|(name, _)| name).collect();
        Ok(Outcome::Select { columns, rows })
    }

    /// Evaluate a HAVING predicate for one group. Aggregate function calls are
    /// computed over the group's members; group-key columns resolve to the key;
    /// comparisons, AND/OR/NOT, and arithmetic compose them.
    fn eval_having(
        expr: &Expr,
        key: &[Value],
        members: &[usize],
        rows_in: &[Vec<Value>],
        group_keys: &[usize],
        cols: &dyn ColumnResolver,
    ) -> Result<Value> {
        use BinaryOperator::*;
        match expr {
            Expr::Nested(inner) => {
                Self::eval_having(inner, key, members, rows_in, group_keys, cols)
            }
            Expr::Value(_) => literal(expr),
            Expr::Function(f) => parse_aggregate(f, cols)?.compute(members, rows_in),
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
                let idx = resolve_col_expr(cols, expr)?;
                let pos = group_keys.iter().position(|k| *k == idx).ok_or_else(|| {
                    SqlError::Unsupported(format!(
                        "HAVING column {expr} must be in GROUP BY or an aggregate"
                    ))
                })?;
                Ok(key[pos].clone())
            }
            Expr::UnaryOp {
                op: UnaryOperator::Not,
                expr: inner,
            } => Ok(Value::Bool(!matches!(
                Self::eval_having(inner, key, members, rows_in, group_keys, cols)?,
                Value::Bool(true)
            ))),
            Expr::BinaryOp { left, op, right } => {
                let l = || Self::eval_having(left, key, members, rows_in, group_keys, cols);
                let r = || Self::eval_having(right, key, members, rows_in, group_keys, cols);
                match op {
                    And => Ok(Value::Bool(
                        matches!(l()?, Value::Bool(true)) && matches!(r()?, Value::Bool(true)),
                    )),
                    Or => Ok(Value::Bool(
                        matches!(l()?, Value::Bool(true)) || matches!(r()?, Value::Bool(true)),
                    )),
                    Plus | Minus | Multiply | Divide | Modulo => arith(op, &l()?, &r()?),
                    Eq | NotEq | Lt | LtEq | Gt | GtEq => {
                        Ok(Value::Bool(compare(op, &l()?, &r()?)))
                    }
                    other => Err(SqlError::Unsupported(format!("operator: {other}"))),
                }
            }
            other => Err(SqlError::Unsupported(format!(
                "HAVING expression: {other:?}"
            ))),
        }
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
        let sec = self.secondary_indexes(&table);
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
            let original = row.clone();
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

            // Secondary UNIQUE indexes: when the composite value changes, reject
            // a visible duplicate in another row before writing.
            for (def, tree) in &sec {
                if !def.unique {
                    continue;
                }
                let new_key = index_row_key(&row, &def.columns)?;
                let old_key = index_row_key(&original, &def.columns)?;
                if let Some(key) = &new_key {
                    if new_key != old_key {
                        if let Some(existing) = tree.search(key)? {
                            if existing != rid && self.store.read(txn, existing)?.is_some() {
                                return Err(SqlError::Constraint(format!(
                                    "duplicate value for UNIQUE index {}",
                                    def.name
                                )));
                            }
                        }
                    }
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
            // Point each secondary index at the new version (a changed value
            // writes a new key; the stale old key is hidden by MVCC on read).
            for (def, tree) in &sec {
                if let Some(key) = index_row_key(&row, &def.columns)? {
                    let stored = if def.unique {
                        key
                    } else {
                        nonunique_key(&key, new_rid)
                    };
                    tree.insert(&stored, new_rid)?;
                }
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

    /// Whether `row` satisfies the boolean predicate `expr`.
    fn matches(&self, expr: &Expr, row: &[Value], cols: &dyn ColumnResolver) -> Result<bool> {
        Ok(matches!(self.eval(expr, row, cols)?, Value::Bool(true)))
    }

    /// Evaluate `expr` against `row`, resolving column references through `cols`.
    fn eval(&self, expr: &Expr, row: &[Value], cols: &dyn ColumnResolver) -> Result<Value> {
        use BinaryOperator::*;
        match expr {
            Expr::Nested(inner) => self.eval(inner, row, cols),
            Expr::Identifier(ident) => {
                let idx = cols.resolve(None, &ident.value)?;
                Ok(row[idx].clone())
            }
            // `t.col` — a qualified reference into a joined row.
            Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
                let idx = cols.resolve(Some(&parts[0].value), &parts[1].value)?;
                Ok(row[idx].clone())
            }
            Expr::Value(_) => literal(expr),
            Expr::UnaryOp { op, expr: inner } => match op {
                UnaryOperator::Not => Ok(Value::Bool(!self.matches(inner, row, cols)?)),
                UnaryOperator::Minus | UnaryOperator::Plus => {
                    match (op, self.eval(inner, row, cols)?) {
                        (_, Value::Null) => Ok(Value::Null),
                        (UnaryOperator::Minus, Value::Int64(n)) => Ok(Value::Int64(-n)),
                        (UnaryOperator::Minus, Value::Double(d)) => Ok(Value::Double(-d)),
                        (UnaryOperator::Plus, v @ (Value::Int64(_) | Value::Double(_))) => Ok(v),
                        (_, other) => Err(SqlError::Type(format!(
                            "cannot apply unary {op} to {other:?}"
                        ))),
                    }
                }
                other => Err(SqlError::Unsupported(format!("unary operator: {other}"))),
            },
            Expr::BinaryOp { left, op, right } => match op {
                And => Ok(Value::Bool(
                    self.matches(left, row, cols)? && self.matches(right, row, cols)?,
                )),
                Or => Ok(Value::Bool(
                    self.matches(left, row, cols)? || self.matches(right, row, cols)?,
                )),
                Plus | Minus | Multiply | Divide | Modulo => {
                    let l = self.eval(left, row, cols)?;
                    let r = self.eval(right, row, cols)?;
                    arith(op, &l, &r)
                }
                Eq | NotEq | Lt | LtEq | Gt | GtEq => {
                    let l = self.eval(left, row, cols)?;
                    let r = self.eval(right, row, cols)?;
                    Ok(Value::Bool(compare(op, &l, &r)))
                }
                other => Err(SqlError::Unsupported(format!("operator: {other}"))),
            },
            Expr::IsNull(inner) => Ok(Value::Bool(matches!(
                self.eval(inner, row, cols)?,
                Value::Null
            ))),
            Expr::IsNotNull(inner) => Ok(Value::Bool(!matches!(
                self.eval(inner, row, cols)?,
                Value::Null
            ))),
            // `v [NOT] IN (a, b, …)`. A NULL probe never matches.
            Expr::InList {
                expr: inner,
                list,
                negated,
            } => {
                let v = self.eval(inner, row, cols)?;
                let mut found = false;
                if !matches!(v, Value::Null) {
                    for item in list {
                        if self.eval(item, row, cols)? == v {
                            found = true;
                            break;
                        }
                    }
                }
                Ok(Value::Bool(found ^ negated))
            }
            // `v [NOT] BETWEEN lo AND hi` — inclusive; NULL operands exclude.
            Expr::Between {
                expr: inner,
                negated,
                low,
                high,
            } => {
                let v = self.eval(inner, row, cols)?;
                let lo = self.eval(low, row, cols)?;
                let hi = self.eval(high, row, cols)?;
                let in_range = compare(&GtEq, &v, &lo) && compare(&LtEq, &v, &hi);
                Ok(Value::Bool(in_range ^ negated))
            }
            // `s [NOT] LIKE pattern` with `%` (any run) and `_` (one char).
            Expr::Like {
                negated,
                any,
                expr: inner,
                pattern,
                escape_char,
            } => {
                if *any || escape_char.is_some() {
                    return Err(SqlError::Unsupported("LIKE ANY / ESCAPE".into()));
                }
                let v = self.eval(inner, row, cols)?;
                let p = self.eval(pattern, row, cols)?;
                let hit = match (&v, &p) {
                    (Value::Text(s), Value::Text(pat)) => like_match(s, pat),
                    (Value::Null, _) | (_, Value::Null) => false,
                    _ => return Err(SqlError::Type("LIKE requires text operands".into())),
                };
                Ok(Value::Bool(hit ^ negated))
            }
            Expr::Function(f) => self.eval_function(f, row, cols),
            // `TRIM(x)` parses to its own node (not a function call).
            Expr::Trim {
                expr: inner,
                trim_where,
                trim_what,
                trim_characters,
            } => {
                if trim_where.is_some() || trim_what.is_some() || trim_characters.is_some() {
                    return Err(SqlError::Unsupported(
                        "TRIM with LEADING/TRAILING or trim characters".into(),
                    ));
                }
                str_map(self.eval(inner, row, cols)?, |s| s.trim().to_string())
            }
            // `CASE [op] WHEN c THEN r … [ELSE e] END`. Searched form (no
            // operand) tests each `c` as a boolean; simple form compares `op`
            // against each `c`. Falls back to `ELSE` (or NULL).
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => {
                match operand {
                    None => {
                        for (cond, res) in conditions.iter().zip(results) {
                            if self.matches(cond, row, cols)? {
                                return self.eval(res, row, cols);
                            }
                        }
                    }
                    Some(op) => {
                        let target = self.eval(op, row, cols)?;
                        for (cond, res) in conditions.iter().zip(results) {
                            if self.eval(cond, row, cols)? == target {
                                return self.eval(res, row, cols);
                            }
                        }
                    }
                }
                match else_result {
                    Some(e) => self.eval(e, row, cols),
                    None => Ok(Value::Null),
                }
            }
            // `INTERVAL n <unit>` → a count of seconds (dates are epoch seconds),
            // so it composes with `DATE_ADD`/`DATE_SUB` and plain arithmetic.
            Expr::Interval(iv) => {
                let n = match self.eval(&iv.value, row, cols)? {
                    Value::Int64(n) => n,
                    Value::Null => return Ok(Value::Null),
                    other => {
                        return Err(SqlError::Type(format!(
                            "INTERVAL value must be an integer, got {other:?}"
                        )));
                    }
                };
                let unit_secs: i64 = match &iv.leading_field {
                    None | Some(DateTimeField::Second | DateTimeField::Seconds) => 1,
                    Some(DateTimeField::Minute | DateTimeField::Minutes) => 60,
                    Some(DateTimeField::Hour | DateTimeField::Hours) => 3_600,
                    Some(DateTimeField::Day | DateTimeField::Days) => 86_400,
                    Some(DateTimeField::Week(_) | DateTimeField::Weeks) => 7 * 86_400,
                    Some(other) => {
                        return Err(SqlError::Unsupported(format!("INTERVAL unit {other:?}")));
                    }
                };
                n.checked_mul(unit_secs)
                    .map(Value::Int64)
                    .ok_or_else(|| SqlError::Type("INTERVAL overflow".into()))
            }
            // `CEIL`/`FLOOR` parse to their own nodes (not function calls).
            // Integer operands are already whole, so both are the identity until
            // a floating-point type lands.
            Expr::Ceil { expr: inner, field } => self.ceil_floor(inner, field, row, cols, true),
            Expr::Floor { expr: inner, field } => self.ceil_floor(inner, field, row, cols, false),
            // `CAST(expr AS <type>)` (and `expr::type`) — convert between scalar
            // types, e.g. `CAST('2021-06-15' AS TIMESTAMP)` or `CAST(x AS DOUBLE)`.
            Expr::Cast {
                expr: inner,
                data_type,
                ..
            } => {
                let v = self.eval(inner, row, cols)?;
                cast_value(v, map_data_type(data_type)?)
            }
            other => Err(SqlError::Unsupported(format!("expression: {other:?}"))),
        }
    }

    /// Shared implementation of `CEIL`/`FLOOR` (which parse to their own AST
    /// nodes). Integers pass through unchanged; doubles round toward +∞ (ceil)
    /// or −∞ (floor).
    fn ceil_floor(
        &self,
        inner: &Expr,
        field: &CeilFloorKind,
        row: &[Value],
        cols: &dyn ColumnResolver,
        is_ceil: bool,
    ) -> Result<Value> {
        match field {
            CeilFloorKind::DateTimeField(DateTimeField::NoDateTime) | CeilFloorKind::Scale(_) => {}
            CeilFloorKind::DateTimeField(_) => {
                return Err(SqlError::Unsupported("CEIL/FLOOR TO <unit>".into()));
            }
        }
        match self.eval(inner, row, cols)? {
            v @ Value::Int64(_) => Ok(v),
            Value::Double(d) => Ok(Value::Double(if is_ceil { d.ceil() } else { d.floor() })),
            Value::Null => Ok(Value::Null),
            other => Err(SqlError::Type(format!(
                "CEIL/FLOOR expects a number, got {other:?}"
            ))),
        }
    }

    /// Evaluate a scalar function call (date/time, string, numeric helpers).
    /// Aggregate names are rejected here — they are handled by the aggregate
    /// path, not per-row evaluation.
    fn eval_function(
        &self,
        f: &Function,
        row: &[Value],
        cols: &dyn ColumnResolver,
    ) -> Result<Value> {
        let name = object_name(&f.name).to_ascii_uppercase();
        if is_aggregate_name(&name) {
            return Err(SqlError::Unsupported(format!(
                "aggregate {name} is not allowed here"
            )));
        }
        let arg_exprs = scalar_args(f)?;
        let arg = |i: usize| -> Result<Value> {
            let e = arg_exprs
                .get(i)
                .ok_or_else(|| SqlError::Type(format!("{name} is missing an argument")))?;
            self.eval(e, row, cols)
        };
        let nargs = arg_exprs.len();
        match name.as_str() {
            // ── date / time (BIGINT operands and results are Unix epoch seconds) ──
            "NOW" | "CURRENT_TIMESTAMP" => Ok(Value::Int64(now_epoch_secs())),
            "YEAR" => date_part(arg(0)?, DatePart::Year),
            "MONTH" => date_part(arg(0)?, DatePart::Month),
            "DAY" => date_part(arg(0)?, DatePart::Day),
            "HOUR" => date_part(arg(0)?, DatePart::Hour),
            "MINUTE" => date_part(arg(0)?, DatePart::Minute),
            "SECOND" => date_part(arg(0)?, DatePart::Second),
            // ── numeric ──
            "ABS" => match arg(0)? {
                Value::Int64(n) => Ok(Value::Int64(n.abs())),
                Value::Double(d) => Ok(Value::Double(d.abs())),
                Value::Null => Ok(Value::Null),
                other => Err(SqlError::Type(format!(
                    "ABS expects a number, got {other:?}"
                ))),
            },
            "MOD" => arith(&BinaryOperator::Modulo, &arg(0)?, &arg(1)?),
            // ── string ──
            "LENGTH" | "CHAR_LENGTH" => match arg(0)? {
                Value::Text(s) => Ok(Value::Int64(s.chars().count() as i64)),
                Value::Null => Ok(Value::Null),
                other => Err(SqlError::Type(format!(
                    "LENGTH expects text, got {other:?}"
                ))),
            },
            "UPPER" => str_map(arg(0)?, |s| s.to_uppercase()),
            "LOWER" => str_map(arg(0)?, |s| s.to_lowercase()),
            "TRIM" => str_map(arg(0)?, |s| s.trim().to_string()),
            "CONCAT" => {
                let mut out = String::new();
                for i in 0..nargs {
                    match arg(i)? {
                        Value::Null => {}
                        Value::Text(s) => out.push_str(&s),
                        Value::Int64(n) => out.push_str(&n.to_string()),
                        Value::Double(d) => out.push_str(&types::format_double(d)),
                        Value::Timestamp(t) => out.push_str(&types::format_timestamp(t)),
                        Value::Bool(b) => out.push_str(if b { "true" } else { "false" }),
                    }
                }
                Ok(Value::Text(out))
            }
            "SUBSTR" | "SUBSTRING" => {
                let s = match arg(0)? {
                    Value::Text(s) => s,
                    Value::Null => return Ok(Value::Null),
                    other => {
                        return Err(SqlError::Type(format!(
                            "SUBSTR expects text, got {other:?}"
                        )));
                    }
                };
                let chars: Vec<char> = s.chars().collect();
                // 1-indexed start (SQL convention); clamp to bounds.
                let start = match arg(1)? {
                    Value::Int64(n) => n,
                    other => {
                        return Err(SqlError::Type(format!(
                            "SUBSTR start must be an integer, got {other:?}"
                        )));
                    }
                };
                let begin = (start.max(1) - 1).min(chars.len() as i64) as usize;
                let end = if nargs >= 3 {
                    match arg(2)? {
                        Value::Int64(len) => {
                            (begin as i64 + len.max(0)).min(chars.len() as i64) as usize
                        }
                        other => {
                            return Err(SqlError::Type(format!(
                                "SUBSTR length must be an integer, got {other:?}"
                            )));
                        }
                    }
                } else {
                    chars.len()
                };
                Ok(Value::Text(chars[begin..end].iter().collect()))
            }
            "COALESCE" => {
                for i in 0..nargs {
                    let v = arg(i)?;
                    if !matches!(v, Value::Null) {
                        return Ok(v);
                    }
                }
                Ok(Value::Null)
            }
            "REPLACE" => match (arg(0)?, arg(1)?, arg(2)?) {
                (Value::Text(s), Value::Text(from), Value::Text(to)) => {
                    Ok(Value::Text(s.replace(&from, &to)))
                }
                (Value::Null, _, _) | (_, Value::Null, _) | (_, _, Value::Null) => Ok(Value::Null),
                _ => Err(SqlError::Type("REPLACE requires text operands".into())),
            },
            // ── more date / time (epoch seconds; see NOW/YEAR above) ──
            "CURDATE" | "CURRENT_DATE" => {
                let now = now_epoch_secs();
                Ok(Value::Int64(now - now.rem_euclid(86_400)))
            }
            "DATEDIFF" => match (arg(0)?, arg(1)?) {
                (Value::Int64(a), Value::Int64(b)) => Ok(Value::Int64((a - b).div_euclid(86_400))),
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (a, b) => Err(SqlError::Type(format!(
                    "DATEDIFF expects integer epoch seconds, got {a:?}, {b:?}"
                ))),
            },
            // The second operand is typically `INTERVAL n <unit>`, which evaluates
            // to a count of seconds; a bare integer (seconds) also works.
            "DATE_ADD" | "ADDDATE" => arith(&BinaryOperator::Plus, &arg(0)?, &arg(1)?),
            "DATE_SUB" | "SUBDATE" => arith(&BinaryOperator::Minus, &arg(0)?, &arg(1)?),
            // ── more numeric (integer semantics until a float type lands) ──
            "ROUND" => match arg(0)? {
                Value::Null => Ok(Value::Null),
                Value::Int64(n) => {
                    // Integers are already whole; a negative scale rounds to a
                    // power of ten (e.g. ROUND(1234, -2) = 1200).
                    let scale = if nargs >= 2 {
                        match arg(1)? {
                            Value::Int64(d) => d,
                            other => {
                                return Err(SqlError::Type(format!(
                                    "ROUND scale must be an integer, got {other:?}"
                                )));
                            }
                        }
                    } else {
                        0
                    };
                    if scale >= 0 {
                        return Ok(Value::Int64(n));
                    }
                    let exp = u32::try_from(scale.unsigned_abs())
                        .map_err(|_| SqlError::Type("ROUND scale too large".into()))?;
                    let factor = 10i64
                        .checked_pow(exp)
                        .ok_or_else(|| SqlError::Type("ROUND scale too large".into()))?;
                    let half = factor / 2;
                    let rounded = if n >= 0 {
                        (n + half) / factor * factor
                    } else {
                        (n - half) / factor * factor
                    };
                    Ok(Value::Int64(rounded))
                }
                Value::Double(d) => {
                    // ROUND(x[, scale]) to `scale` decimal places (default 0).
                    let scale = if nargs >= 2 {
                        match arg(1)? {
                            Value::Int64(s) => s,
                            other => {
                                return Err(SqlError::Type(format!(
                                    "ROUND scale must be an integer, got {other:?}"
                                )));
                            }
                        }
                    } else {
                        0
                    };
                    let factor = 10f64.powi(scale as i32);
                    Ok(Value::Double((d * factor).round() / factor))
                }
                other => Err(SqlError::Type(format!(
                    "ROUND expects a number, got {other:?}"
                ))),
            },
            "POW" | "POWER" => match (arg(0)?, arg(1)?) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Int64(base), Value::Int64(exp)) if exp >= 0 => {
                    let e = u32::try_from(exp).expect("exp >= 0");
                    base.checked_pow(e)
                        .map(Value::Int64)
                        .ok_or_else(|| SqlError::Type("POW overflow".into()))
                }
                // A negative exponent or any double operand yields a double.
                (a, b) => match (as_f64(&a), as_f64(&b)) {
                    (Some(base), Some(exp)) => Ok(Value::Double(base.powf(exp))),
                    _ => Err(SqlError::Type(format!(
                        "POW expects numbers, got {a:?}, {b:?}"
                    ))),
                },
            },
            // ── control flow ──
            "IF" => {
                if nargs != 3 {
                    return Err(SqlError::Type("IF takes exactly three arguments".into()));
                }
                if self.matches(arg_exprs[0], row, cols)? {
                    self.eval(arg_exprs[1], row, cols)
                } else {
                    self.eval(arg_exprs[2], row, cols)
                }
            }
            "IFNULL" => {
                let a = arg(0)?;
                if matches!(a, Value::Null) {
                    arg(1)
                } else {
                    Ok(a)
                }
            }
            "NULLIF" => {
                let a = arg(0)?;
                if a == arg(1)? { Ok(Value::Null) } else { Ok(a) }
            }
            // ── more string ──
            "LEFT" => match (arg(0)?, arg(1)?) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(s), Value::Int64(n)) => {
                    Ok(Value::Text(s.chars().take(n.max(0) as usize).collect()))
                }
                (a, b) => Err(SqlError::Type(format!(
                    "LEFT expects (text, int), got {a:?}, {b:?}"
                ))),
            },
            "RIGHT" => match (arg(0)?, arg(1)?) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(s), Value::Int64(n)) => {
                    let chars: Vec<char> = s.chars().collect();
                    let take = (n.max(0) as usize).min(chars.len());
                    Ok(Value::Text(chars[chars.len() - take..].iter().collect()))
                }
                (a, b) => Err(SqlError::Type(format!(
                    "RIGHT expects (text, int), got {a:?}, {b:?}"
                ))),
            },
            "REVERSE" => str_map(arg(0)?, |s| s.chars().rev().collect()),
            "REPEAT" => match (arg(0)?, arg(1)?) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(s), Value::Int64(n)) => Ok(Value::Text(s.repeat(n.max(0) as usize))),
                (a, b) => Err(SqlError::Type(format!(
                    "REPEAT expects (text, int), got {a:?}, {b:?}"
                ))),
            },
            "SPACE" => match arg(0)? {
                Value::Null => Ok(Value::Null),
                Value::Int64(n) => Ok(Value::Text(" ".repeat(n.max(0) as usize))),
                other => Err(SqlError::Type(format!(
                    "SPACE expects an int, got {other:?}"
                ))),
            },
            "LPAD" | "RPAD" => match (arg(0)?, arg(1)?, arg(2)?) {
                (Value::Null, _, _) | (_, Value::Null, _) | (_, _, Value::Null) => Ok(Value::Null),
                (Value::Text(s), Value::Int64(len), Value::Text(pad)) => {
                    let target = len.max(0) as usize;
                    let chars: Vec<char> = s.chars().collect();
                    if chars.len() >= target {
                        return Ok(Value::Text(chars[..target].iter().collect()));
                    }
                    let pad_chars: Vec<char> = pad.chars().collect();
                    if pad_chars.is_empty() {
                        return Ok(Value::Text(s));
                    }
                    let fill: String = pad_chars
                        .iter()
                        .cycle()
                        .take(target - chars.len())
                        .collect();
                    Ok(Value::Text(if name == "LPAD" {
                        format!("{fill}{s}")
                    } else {
                        format!("{s}{fill}")
                    }))
                }
                (a, b, c) => Err(SqlError::Type(format!(
                    "{name} expects (text, int, text), got {a:?}, {b:?}, {c:?}"
                ))),
            },
            "INSTR" => match (arg(0)?, arg(1)?) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(s), Value::Text(sub)) => Ok(Value::Int64(str_index(&s, &sub))),
                (a, b) => Err(SqlError::Type(format!(
                    "INSTR expects text, got {a:?}, {b:?}"
                ))),
            },
            "LOCATE" => match (arg(0)?, arg(1)?) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                // LOCATE takes (substring, string) — the reverse of INSTR.
                (Value::Text(sub), Value::Text(s)) => Ok(Value::Int64(str_index(&s, &sub))),
                (a, b) => Err(SqlError::Type(format!(
                    "LOCATE expects text, got {a:?}, {b:?}"
                ))),
            },
            "ASCII" => match arg(0)? {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Int64(s.chars().next().map_or(0, |c| c as i64))),
                other => Err(SqlError::Type(format!("ASCII expects text, got {other:?}"))),
            },
            // ── more numeric (all but SIGN return DOUBLE) ──
            "SQRT" => num_unary("SQRT", arg(0)?, f64::sqrt),
            "EXP" => num_unary("EXP", arg(0)?, f64::exp),
            "LN" => num_unary("LN", arg(0)?, f64::ln),
            "LOG10" => num_unary("LOG10", arg(0)?, f64::log10),
            "LOG2" => num_unary("LOG2", arg(0)?, f64::log2),
            // LOG(x) is the natural log; LOG(base, x) is the log to that base.
            "LOG" => {
                if nargs >= 2 {
                    match (arg(0)?, arg(1)?) {
                        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                        (a, b) => match (as_f64(&a), as_f64(&b)) {
                            (Some(base), Some(x)) => Ok(Value::Double(x.log(base))),
                            _ => Err(SqlError::Type(format!(
                                "LOG expects numbers, got {a:?}, {b:?}"
                            ))),
                        },
                    }
                } else {
                    num_unary("LOG", arg(0)?, f64::ln)
                }
            }
            "SIGN" => match arg(0)? {
                Value::Null => Ok(Value::Null),
                other => match as_f64(&other) {
                    Some(x) => Ok(Value::Int64(if x > 0.0 {
                        1
                    } else if x < 0.0 {
                        -1
                    } else {
                        0
                    })),
                    None => Err(SqlError::Type(format!(
                        "SIGN expects a number, got {other:?}"
                    ))),
                },
            },
            "TRUNCATE" => match (arg(0)?, arg(1)?) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (a, Value::Int64(d)) => match as_f64(&a) {
                    Some(x) => {
                        let factor = 10f64.powi(d as i32);
                        Ok(Value::Double((x * factor).trunc() / factor))
                    }
                    None => Err(SqlError::Type(format!(
                        "TRUNCATE expects a number, got {a:?}"
                    ))),
                },
                (a, b) => Err(SqlError::Type(format!(
                    "TRUNCATE expects (number, int), got {a:?}, {b:?}"
                ))),
            },
            "PI" => Ok(Value::Double(std::f64::consts::PI)),
            "GREATEST" | "LEAST" => {
                use std::cmp::Ordering;
                if nargs == 0 {
                    return Err(SqlError::Type(format!(
                        "{name} needs at least one argument"
                    )));
                }
                let mut best: Option<Value> = None;
                let want_greatest = name == "GREATEST";
                for i in 0..nargs {
                    let v = arg(i)?;
                    if matches!(v, Value::Null) {
                        return Ok(Value::Null);
                    }
                    best = Some(match best {
                        None => v,
                        Some(cur) => {
                            let ord = value_cmp(&v, &cur);
                            let take = if want_greatest {
                                ord == Ordering::Greater
                            } else {
                                ord == Ordering::Less
                            };
                            if take { v } else { cur }
                        }
                    });
                }
                Ok(best.expect("at least one argument"))
            }
            // ── more date / time (epoch seconds; see NOW/YEAR above) ──
            "QUARTER" => match date_part(arg(0)?, DatePart::Month)? {
                Value::Int64(m) => Ok(Value::Int64((m - 1) / 3 + 1)),
                other => Ok(other),
            },
            "DAYOFWEEK" => match epoch_seconds_of(arg(0)?)? {
                // 1 = Sunday … 7 = Saturday (1970-01-01 was a Thursday).
                Some(secs) => Ok(Value::Int64(
                    (secs.div_euclid(86_400) + 4).rem_euclid(7) + 1,
                )),
                None => Ok(Value::Null),
            },
            "DAYOFYEAR" => match epoch_seconds_of(arg(0)?)? {
                Some(secs) => {
                    let (y, mo, d, ..) = civil_from_epoch_secs(secs);
                    let doy =
                        types::days_from_civil(y, mo, d) - types::days_from_civil(y, 1, 1) + 1;
                    Ok(Value::Int64(doy))
                }
                None => Ok(Value::Null),
            },
            "UNIX_TIMESTAMP" => {
                let v = if nargs == 0 {
                    Value::Int64(now_epoch_secs())
                } else {
                    arg(0)?
                };
                match epoch_seconds_of(v)? {
                    Some(secs) => Ok(Value::Int64(secs)),
                    None => Ok(Value::Null),
                }
            }
            "FROM_UNIXTIME" => match arg(0)? {
                Value::Null => Ok(Value::Null),
                Value::Int64(secs) => Ok(Value::Timestamp(secs * 1_000_000)),
                other => Err(SqlError::Type(format!(
                    "FROM_UNIXTIME expects epoch seconds, got {other:?}"
                ))),
            },
            other => Err(SqlError::Unsupported(format!("function {other}"))),
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
        (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
        // Numeric comparison across int/double (e.g. `d > 3`).
        _ => match (as_f64(l), as_f64(r)) {
            (Some(x), Some(y)) => match x.partial_cmp(&y) {
                Some(o) => o,
                None => return false, // NaN: never matches
            },
            _ => return false, // type mismatch: never matches
        },
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

/// How a projected output column is produced.
enum ProjKind {
    /// A direct column index into the (joined) row — used for `*` expansion so a
    /// duplicate column name across joined tables is never re-resolved by name.
    Col(usize),
    /// An expression evaluated per row.
    Expr(Expr),
}

/// One output column of a (non-aggregate) `SELECT`: how to produce it, plus the
/// name to report for it.
struct ProjItem {
    name: String,
    kind: ProjKind,
}

/// Resolve a select list to projection items. `*` expands to one item per column
/// (across all joined tables, in order); `t.*` to that table's columns;
/// `expr AS alias` takes the alias; a bare column or expression is named after
/// itself.
fn resolve_projection(items: &[SelectItem], cols: &dyn ColumnResolver) -> Result<Vec<ProjItem>> {
    let all = cols.columns();
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard(_) => {
                for (i, (_q, name)) in all.iter().enumerate() {
                    out.push(ProjItem {
                        name: name.clone(),
                        kind: ProjKind::Col(i),
                    });
                }
            }
            SelectItem::QualifiedWildcard(obj, _) => {
                let q = object_name(obj);
                let mut any = false;
                for (i, (cq, name)) in all.iter().enumerate() {
                    if cq.as_deref() == Some(q.as_str()) {
                        out.push(ProjItem {
                            name: name.clone(),
                            kind: ProjKind::Col(i),
                        });
                        any = true;
                    }
                }
                if !any {
                    return Err(SqlError::NoSuchTable(q));
                }
            }
            SelectItem::UnnamedExpr(expr) => {
                let name = match expr {
                    Expr::Identifier(id) => id.value.clone(),
                    Expr::CompoundIdentifier(parts) if !parts.is_empty() => {
                        parts.last().unwrap().value.clone()
                    }
                    other => other.to_string(),
                };
                out.push(ProjItem {
                    name,
                    kind: ProjKind::Expr(expr.clone()),
                });
            }
            SelectItem::ExprWithAlias { expr, alias } => out.push(ProjItem {
                name: alias.value.clone(),
                kind: ProjKind::Expr(expr.clone()),
            }),
        }
    }
    Ok(out)
}

/// A resolver from a column reference (optional table qualifier + name) to its
/// index in a row. Implemented by a single [`Table`] and by a joined
/// [`JoinSchema`].
trait ColumnResolver {
    /// Resolve `[qualifier.]name` to a row index, or error (not found / ambiguous).
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<usize>;
    /// Each output column as `(qualifier, name)`, in row order (for `*`).
    fn columns(&self) -> Vec<(Option<String>, String)>;
}

impl ColumnResolver for Table {
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<usize> {
        if let Some(q) = qualifier {
            if q != self.name {
                return Err(SqlError::NoSuchColumn(format!("{q}.{name}")));
            }
        }
        self.column_index(name)
            .ok_or_else(|| SqlError::NoSuchColumn(name.to_string()))
    }

    fn columns(&self) -> Vec<(Option<String>, String)> {
        self.columns
            .iter()
            .map(|c| (Some(self.name.clone()), c.name.clone()))
            .collect()
    }
}

/// One column of a joined row: which source it came from, and its name.
struct JoinCol {
    qualifier: String,
    name: String,
}

/// The schema of a (possibly joined) row: a flat, ordered list of columns, each
/// tagged with the source table's alias/name so `t.col` resolves unambiguously.
struct JoinSchema {
    cols: Vec<JoinCol>,
}

impl JoinSchema {
    /// The schema of a single base table under `qualifier` (its alias or name).
    fn single(qualifier: &str, table: &Table) -> Self {
        JoinSchema {
            cols: table
                .columns
                .iter()
                .map(|c| JoinCol {
                    qualifier: qualifier.to_string(),
                    name: c.name.clone(),
                })
                .collect(),
        }
    }
}

impl ColumnResolver for JoinSchema {
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<usize> {
        let mut hit = None;
        for (i, c) in self.cols.iter().enumerate() {
            let q_ok = qualifier.is_none_or(|q| q == c.qualifier);
            if q_ok && c.name == name {
                if hit.is_some() {
                    return Err(SqlError::Unsupported(format!(
                        "ambiguous column reference '{name}' (qualify it with a table name)"
                    )));
                }
                hit = Some(i);
            }
        }
        hit.ok_or_else(|| match qualifier {
            Some(q) => SqlError::NoSuchColumn(format!("{q}.{name}")),
            None => SqlError::NoSuchColumn(name.to_string()),
        })
    }

    fn columns(&self) -> Vec<(Option<String>, String)> {
        self.cols
            .iter()
            .map(|c| (Some(c.qualifier.clone()), c.name.clone()))
            .collect()
    }
}

/// Convert a computed value back into a literal expression, for substituting a
/// resolved subquery result into the surrounding query. (A `Timestamp` reduces
/// to its integer microseconds, losing the timestamp type — a known limit.)
fn value_to_expr(v: Value) -> Expr {
    Expr::Value(match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Boolean(b),
        Value::Int64(n) => SqlValue::Number(n.to_string(), false),
        // `{:?}` keeps a decimal point so it re-parses as a double.
        Value::Double(d) => SqlValue::Number(format!("{d:?}"), false),
        Value::Timestamp(t) => SqlValue::Number(t.to_string(), false),
        Value::Text(s) => SqlValue::SingleQuotedString(s),
    })
}

/// Whether a `SELECT`'s WHERE / projection / HAVING contains a subquery worth a
/// resolve pass (so the common subquery-free query skips the AST clone).
fn select_has_subquery(s: &Select) -> bool {
    s.selection.as_ref().is_some_and(expr_has_subquery)
        || s.having.as_ref().is_some_and(expr_has_subquery)
        || s.projection.iter().any(|item| match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                expr_has_subquery(e)
            }
            _ => false,
        })
}

/// Whether `(qualifier, name)` is among a query's local `FROM` columns. A bare
/// name matches any qualifier.
fn is_local_column(local: &[(String, String)], qualifier: Option<&str>, name: &str) -> bool {
    local
        .iter()
        .any(|(q, n)| n == name && qualifier.is_none_or(|qq| qq == q))
}

/// Collect the column references in `expr` (not descending into subqueries), as
/// `(qualifier, name)`. Used to detect correlation.
fn column_refs(expr: &Expr) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    column_refs_into(expr, &mut out);
    out
}

fn column_refs_into(expr: &Expr, out: &mut Vec<(Option<String>, String)>) {
    match expr {
        Expr::Identifier(id) => out.push((None, id.value.clone())),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            out.push((Some(parts[0].value.clone()), parts[1].value.clone()));
        }
        Expr::Nested(e)
        | Expr::IsNull(e)
        | Expr::IsNotNull(e)
        | Expr::UnaryOp { expr: e, .. }
        | Expr::Cast { expr: e, .. }
        | Expr::Trim { expr: e, .. }
        | Expr::InSubquery { expr: e, .. } => column_refs_into(e, out),
        Expr::BinaryOp { left, right, .. } => {
            column_refs_into(left, out);
            column_refs_into(right, out);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            column_refs_into(expr, out);
            column_refs_into(low, out);
            column_refs_into(high, out);
        }
        Expr::Like { expr, pattern, .. } => {
            column_refs_into(expr, out);
            column_refs_into(pattern, out);
        }
        Expr::InList { expr, list, .. } => {
            column_refs_into(expr, out);
            for e in list {
                column_refs_into(e, out);
            }
        }
        Expr::Function(f) => {
            if let FunctionArguments::List(list) = &f.args {
                for a in &list.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = a {
                        column_refs_into(e, out);
                    }
                }
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(e) = operand {
                column_refs_into(e, out);
            }
            for e in conditions.iter().chain(results) {
                column_refs_into(e, out);
            }
            if let Some(e) = else_result {
                column_refs_into(e, out);
            }
        }
        _ => {}
    }
}

/// Whether `expr` contains a scalar/`IN`/`EXISTS` subquery (recursively).
fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => true,
        Expr::Nested(x) | Expr::IsNull(x) | Expr::IsNotNull(x) | Expr::UnaryOp { expr: x, .. } => {
            expr_has_subquery(x)
        }
        Expr::BinaryOp { left, right, .. } => expr_has_subquery(left) || expr_has_subquery(right),
        Expr::Between {
            expr, low, high, ..
        } => expr_has_subquery(expr) || expr_has_subquery(low) || expr_has_subquery(high),
        Expr::InList { expr, list, .. } => {
            expr_has_subquery(expr) || list.iter().any(expr_has_subquery)
        }
        _ => false,
    }
}

/// Collect `column = literal` equalities reachable through a top-level `AND`
/// chain (either operand order), as `(column name, value)` pairs. Used to pick
/// an index seek; conditions it can't read are simply ignored (the full
/// predicate is still applied to any seeked row).
fn collect_equalities(expr: &Expr) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    collect_equalities_into(expr, &mut out);
    out
}

fn collect_equalities_into(expr: &Expr, out: &mut Vec<(String, Value)>) {
    match expr {
        Expr::Nested(e) => collect_equalities_into(e, out),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            collect_equalities_into(left, out);
            collect_equalities_into(right, out);
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => match (left.as_ref(), right.as_ref()) {
            (Expr::Identifier(id), other) | (other, Expr::Identifier(id)) => {
                if let Ok(v) = literal(other) {
                    out.push((id.value.clone(), v));
                }
            }
            _ => {}
        },
        _ => {}
    }
}

/// A range comparison against a column, for primary-key range seeks.
#[derive(Clone, Copy)]
enum RangeOp {
    Gt,
    Ge,
    Lt,
    Le,
}

/// Collect `col <op> literal` range constraints (`>`, `>=`, `<`, `<=`) and
/// `col BETWEEN lo AND hi` from a top-level `AND` chain. A `literal <op> col`
/// form flips the operator. Used to seek the primary-key index over a range.
fn collect_ranges(expr: &Expr) -> Vec<(String, RangeOp, Value)> {
    let mut out = Vec::new();
    collect_ranges_into(expr, &mut out);
    out
}

fn collect_ranges_into(expr: &Expr, out: &mut Vec<(String, RangeOp, Value)>) {
    match expr {
        Expr::Nested(e) => collect_ranges_into(e, out),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            collect_ranges_into(left, out);
            collect_ranges_into(right, out);
        }
        Expr::BinaryOp { left, op, right } => {
            // (operator when `col <op> literal`, operator when `literal <op> col`).
            let ops = match op {
                BinaryOperator::Gt => Some((RangeOp::Gt, RangeOp::Lt)),
                BinaryOperator::GtEq => Some((RangeOp::Ge, RangeOp::Le)),
                BinaryOperator::Lt => Some((RangeOp::Lt, RangeOp::Gt)),
                BinaryOperator::LtEq => Some((RangeOp::Le, RangeOp::Ge)),
                _ => None,
            };
            if let Some((forward, flipped)) = ops {
                match (left.as_ref(), right.as_ref()) {
                    (Expr::Identifier(id), other) => {
                        if let Ok(v) = literal(other) {
                            out.push((id.value.clone(), forward, v));
                        }
                    }
                    (other, Expr::Identifier(id)) => {
                        if let Ok(v) = literal(other) {
                            out.push((id.value.clone(), flipped, v));
                        }
                    }
                    _ => {}
                }
            }
        }
        Expr::Between {
            expr,
            negated: false,
            low,
            high,
        } => {
            if let Expr::Identifier(id) = expr.as_ref() {
                if let Ok(v) = literal(low) {
                    out.push((id.value.clone(), RangeOp::Ge, v));
                }
                if let Ok(v) = literal(high) {
                    out.push((id.value.clone(), RangeOp::Le, v));
                }
            }
        }
        _ => {}
    }
}

/// Expand a query's `WITH` clause by inlining each CTE as a derived table
/// wherever its name is referenced (non-recursive). CTEs may reference earlier
/// ones; a self- or forward-reference is left unresolved, so a recursive CTE
/// fails at table lookup rather than looping. A CTE column-rename list
/// (`WITH c(a, b) AS …`) is not applied — references use the body's output names.
fn expand_ctes(query: &mut Query) {
    let Some(with) = query.with.take() else {
        return;
    };
    let mut ctes: Vec<(String, Query, TableAlias)> = Vec::new();
    for cte in with.cte_tables {
        let name = cte.alias.name.value.clone();
        let mut body = (*cte.query).clone();
        inline_ctes_into_query(&mut body, &ctes);
        ctes.push((name, body, cte.alias));
    }
    inline_ctes_into_query(query, &ctes);
}

fn inline_ctes_into_query(q: &mut Query, ctes: &[(String, Query, TableAlias)]) {
    inline_ctes_into_setexpr(&mut q.body, ctes);
}

fn inline_ctes_into_setexpr(body: &mut SetExpr, ctes: &[(String, Query, TableAlias)]) {
    match body {
        SetExpr::Select(s) => inline_ctes_into_select(s, ctes),
        SetExpr::Query(q) => inline_ctes_into_query(q, ctes),
        SetExpr::SetOperation { left, right, .. } => {
            inline_ctes_into_setexpr(left, ctes);
            inline_ctes_into_setexpr(right, ctes);
        }
        _ => {}
    }
}

fn inline_ctes_into_select(s: &mut Select, ctes: &[(String, Query, TableAlias)]) {
    for twj in &mut s.from {
        inline_ctes_into_factor(&mut twj.relation, ctes);
        for join in &mut twj.joins {
            inline_ctes_into_factor(&mut join.relation, ctes);
        }
    }
    if let Some(e) = &mut s.selection {
        inline_ctes_into_expr(e, ctes);
    }
    if let Some(e) = &mut s.having {
        inline_ctes_into_expr(e, ctes);
    }
    for item in &mut s.projection {
        if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item {
            inline_ctes_into_expr(e, ctes);
        }
    }
}

fn inline_ctes_into_factor(factor: &mut TableFactor, ctes: &[(String, Query, TableAlias)]) {
    match factor {
        TableFactor::Table { name, alias, .. } => {
            let key = object_name(name);
            if let Some((_, body, cte_alias)) = ctes.iter().find(|(n, _, _)| *n == key) {
                let use_alias = alias.clone().unwrap_or_else(|| cte_alias.clone());
                *factor = TableFactor::Derived {
                    lateral: false,
                    subquery: Box::new(body.clone()),
                    alias: Some(use_alias),
                };
            }
        }
        TableFactor::Derived { subquery, .. } => inline_ctes_into_query(subquery, ctes),
        _ => {}
    }
}

fn inline_ctes_into_expr(e: &mut Expr, ctes: &[(String, Query, TableAlias)]) {
    match e {
        Expr::Subquery(q) | Expr::Exists { subquery: q, .. } => inline_ctes_into_query(q, ctes),
        Expr::InSubquery { subquery, .. } => inline_ctes_into_query(subquery, ctes),
        Expr::Nested(inner) | Expr::UnaryOp { expr: inner, .. } => {
            inline_ctes_into_expr(inner, ctes)
        }
        Expr::BinaryOp { left, right, .. } => {
            inline_ctes_into_expr(left, ctes);
            inline_ctes_into_expr(right, ctes);
        }
        _ => {}
    }
}

/// Resolve a column-reference expression (`col` or `t.col`) to a row index.
fn resolve_col_expr(cols: &dyn ColumnResolver, expr: &Expr) -> Result<usize> {
    match expr {
        Expr::Identifier(id) => cols.resolve(None, &id.value),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            cols.resolve(Some(&parts[0].value), &parts[1].value)
        }
        other => Err(SqlError::Unsupported(format!(
            "column reference: {other:?}"
        ))),
    }
}

/// Cross-join (cartesian product) of two materialized row sets, concatenating
/// their schemas. Used for comma-separated `FROM` items and as the seed step.
fn cross_join(
    mut left_schema: JoinSchema,
    left_rows: Vec<Vec<Value>>,
    right_schema: JoinSchema,
    right_rows: Vec<Vec<Value>>,
) -> (JoinSchema, Vec<Vec<Value>>) {
    let mut out = Vec::with_capacity(left_rows.len().saturating_mul(right_rows.len()));
    for l in &left_rows {
        for r in &right_rows {
            let mut row = Vec::with_capacity(l.len() + r.len());
            row.extend_from_slice(l);
            row.extend_from_slice(r);
            out.push(row);
        }
    }
    left_schema.cols.extend(right_schema.cols);
    (left_schema, out)
}

/// The `ON <expr>` of a join constraint, `None` for a constraint-free (cross)
/// join. `USING` / `NATURAL` are not supported yet.
/// A resolved join condition. `Pairs` holds `(left index, right index)` equalities
/// from `USING`/`NATURAL`, which also drives column coalescing.
enum JoinCond<'e> {
    Always,
    On(&'e Expr),
    Pairs(Vec<(usize, usize)>),
}

/// Coalesce the columns of a `USING`/`NATURAL` join: each join column appears
/// once (taking the non-null side), followed by the remaining left then right
/// columns. `pairs` are `(left index, right index)`; `left_w` is the left width.
fn coalesce_join(
    combined: JoinSchema,
    rows: Vec<Vec<Value>>,
    pairs: &[(usize, usize)],
    left_w: usize,
) -> (JoinSchema, Vec<Vec<Value>>) {
    use std::collections::HashSet;
    let left_join: HashSet<usize> = pairs.iter().map(|&(li, _)| li).collect();
    let right_join: HashSet<usize> = pairs.iter().map(|&(_, ri)| ri).collect();

    // A plan describing each output column's source in the combined row.
    enum Src {
        Copy(usize),
        Coalesce(usize, usize),
    }
    let mut cols: Vec<JoinCol> = Vec::new();
    let mut plan: Vec<Src> = Vec::new();

    // Join columns first, coalesced, keeping the left side's qualifier + name.
    for &(li, ri) in pairs {
        let c = &combined.cols[li];
        cols.push(JoinCol {
            qualifier: c.qualifier.clone(),
            name: c.name.clone(),
        });
        plan.push(Src::Coalesce(li, left_w + ri));
    }
    // Remaining left columns, then remaining right columns.
    for i in 0..left_w {
        if !left_join.contains(&i) {
            cols.push(JoinCol {
                qualifier: combined.cols[i].qualifier.clone(),
                name: combined.cols[i].name.clone(),
            });
            plan.push(Src::Copy(i));
        }
    }
    for ri in 0..(combined.cols.len() - left_w) {
        if !right_join.contains(&ri) {
            let i = left_w + ri;
            cols.push(JoinCol {
                qualifier: combined.cols[i].qualifier.clone(),
                name: combined.cols[i].name.clone(),
            });
            plan.push(Src::Copy(i));
        }
    }

    let new_rows = rows
        .into_iter()
        .map(|row| {
            plan.iter()
                .map(|s| match s {
                    Src::Copy(i) => row[*i].clone(),
                    Src::Coalesce(li, ri) => {
                        if matches!(row[*li], Value::Null) {
                            row[*ri].clone()
                        } else {
                            row[*li].clone()
                        }
                    }
                })
                .collect()
        })
        .collect();
    (JoinSchema { cols }, new_rows)
}

/// Remove duplicate rows in place, preserving first-seen order (for DISTINCT).
fn dedup_rows(rows: &mut Vec<Vec<Value>>) {
    let mut seen = std::collections::HashSet::new();
    rows.retain(|row| seen.insert(row.clone()));
}

/// Multiplicity of each distinct row (for multiset `ALL` set operations).
fn row_counts(rows: Vec<Vec<Value>>) -> std::collections::HashMap<Vec<Value>, usize> {
    let mut counts = std::collections::HashMap::new();
    for row in rows {
        *counts.entry(row).or_insert(0usize) += 1;
    }
    counts
}

/// `UNION` (distinct) / `UNION ALL`: the rows of both sides, deduplicated unless
/// `all`. Left rows precede right rows.
fn set_union(mut left: Vec<Vec<Value>>, right: Vec<Vec<Value>>, all: bool) -> Vec<Vec<Value>> {
    left.extend(right);
    if !all {
        dedup_rows(&mut left);
    }
    left
}

/// `INTERSECT` (distinct) / `INTERSECT ALL`: rows on both sides. `ALL` keeps
/// `min(countL, countR)` copies; otherwise the distinct intersection. Order
/// follows the left side.
fn set_intersect(left: Vec<Vec<Value>>, right: Vec<Vec<Value>>, all: bool) -> Vec<Vec<Value>> {
    let mut right_counts = row_counts(right);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in left {
        if all {
            if let Some(c) = right_counts.get_mut(&row) {
                if *c > 0 {
                    *c -= 1;
                    out.push(row);
                }
            }
        } else if right_counts.contains_key(&row) && seen.insert(row.clone()) {
            out.push(row);
        }
    }
    out
}

/// `EXCEPT` (distinct) / `EXCEPT ALL`: left rows not matched on the right. `ALL`
/// removes one left copy per matching right copy; otherwise the distinct
/// difference. Order follows the left side.
fn set_except(left: Vec<Vec<Value>>, right: Vec<Vec<Value>>, all: bool) -> Vec<Vec<Value>> {
    let mut right_counts = row_counts(right);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in left {
        if all {
            match right_counts.get_mut(&row) {
                Some(c) if *c > 0 => *c -= 1,
                _ => out.push(row),
            }
        } else if !right_counts.contains_key(&row) && seen.insert(row.clone()) {
            out.push(row);
        }
    }
    out
}

/// Extract a scalar function's positional argument expressions.
fn scalar_args(f: &Function) -> Result<Vec<&Expr>> {
    match &f.args {
        FunctionArguments::None => Ok(Vec::new()),
        FunctionArguments::List(list) => {
            let mut out = Vec::with_capacity(list.args.len());
            for a in &list.args {
                match a {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => out.push(e),
                    other => {
                        return Err(SqlError::Unsupported(format!(
                            "function argument: {other:?}"
                        )));
                    }
                }
            }
            Ok(out)
        }
        other => Err(SqlError::Unsupported(format!(
            "function arguments: {other:?}"
        ))),
    }
}

/// Apply `f` to a text value, propagating NULL and rejecting non-text.
fn str_map(v: Value, f: impl FnOnce(&str) -> String) -> Result<Value> {
    match v {
        Value::Text(s) => Ok(Value::Text(f(&s))),
        Value::Null => Ok(Value::Null),
        other => Err(SqlError::Type(format!("expected text, got {other:?}"))),
    }
}

/// Apply a floating-point unary function, propagating NULL and rejecting
/// non-numeric operands. The result is always a `DOUBLE`.
fn num_unary(name: &str, v: Value, f: impl FnOnce(f64) -> f64) -> Result<Value> {
    match v {
        Value::Null => Ok(Value::Null),
        other => match as_f64(&other) {
            Some(x) => Ok(Value::Double(f(x))),
            None => Err(SqlError::Type(format!(
                "{name} expects a number, got {other:?}"
            ))),
        },
    }
}

/// The 1-based character position of `needle` in `haystack`, or 0 if absent. An
/// empty needle is found at position 1 (MySQL's `INSTR`/`LOCATE` convention).
fn str_index(haystack: &str, needle: &str) -> i64 {
    if needle.is_empty() {
        return 1;
    }
    match haystack.find(needle) {
        Some(byte) => haystack[..byte].chars().count() as i64 + 1,
        None => 0,
    }
}

/// Current Unix time in whole seconds.
fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A date/time component to extract.
#[derive(Clone, Copy)]
enum DatePart {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

/// Extract a date/time component from a Unix-epoch-seconds BIGINT (UTC).
fn date_part(v: Value, part: DatePart) -> Result<Value> {
    let secs = match v {
        Value::Int64(n) => n,
        // A TIMESTAMP is microseconds; reduce to whole seconds for the calendar.
        Value::Timestamp(micros) => micros.div_euclid(1_000_000),
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(SqlError::Type(format!(
                "date function expects a timestamp or epoch-seconds integer, got {other:?}"
            )));
        }
    };
    let (y, mo, d, h, mi, s) = civil_from_epoch_secs(secs);
    Ok(Value::Int64(match part {
        DatePart::Year => y,
        DatePart::Month => mo,
        DatePart::Day => d,
        DatePart::Hour => h,
        DatePart::Minute => mi,
        DatePart::Second => s,
    }))
}

/// Reduce a date/time value to whole Unix epoch seconds (UTC). A `TIMESTAMP`
/// (epoch microseconds) is divided down to seconds; NULL maps to `None`.
fn epoch_seconds_of(v: Value) -> Result<Option<i64>> {
    match v {
        Value::Int64(n) => Ok(Some(n)),
        Value::Timestamp(micros) => Ok(Some(micros.div_euclid(1_000_000))),
        Value::Null => Ok(None),
        other => Err(SqlError::Type(format!(
            "date function expects a timestamp or epoch-seconds integer, got {other:?}"
        ))),
    }
}

/// Convert Unix epoch seconds (UTC) to `(year, month, day, hour, minute,
/// second)` using Howard Hinnant's civil-from-days algorithm.
fn civil_from_epoch_secs(secs: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let hour = sod / 3600;
    let minute = (sod % 3600) / 60;
    let second = sod % 60;
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day, hour, minute, second)
}

/// Apply an arithmetic operator to two integer values, propagating NULL and
/// rejecting non-integers, division/modulo by zero, and overflow.
fn arith(op: &BinaryOperator, l: &Value, r: &Value) -> Result<Value> {
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    // Two integers stay integers (checked); anything with a double promotes to
    // floating point.
    if let (Value::Int64(a), Value::Int64(b)) = (l, r) {
        let out = match op {
            BinaryOperator::Plus => a.checked_add(*b),
            BinaryOperator::Minus => a.checked_sub(*b),
            BinaryOperator::Multiply => a.checked_mul(*b),
            BinaryOperator::Divide if *b == 0 => {
                return Err(SqlError::Type("division by zero".into()));
            }
            BinaryOperator::Modulo if *b == 0 => {
                return Err(SqlError::Type("modulo by zero".into()));
            }
            BinaryOperator::Divide => a.checked_div(*b),
            BinaryOperator::Modulo => a.checked_rem(*b),
            other => return Err(SqlError::Unsupported(format!("operator: {other}"))),
        };
        return out
            .map(Value::Int64)
            .ok_or_else(|| SqlError::Type("integer overflow".into()));
    }
    let (Some(a), Some(b)) = (as_f64(l), as_f64(r)) else {
        return Err(SqlError::Type(format!(
            "arithmetic requires numbers: {l:?} {op} {r:?}"
        )));
    };
    let out = match op {
        BinaryOperator::Plus => a + b,
        BinaryOperator::Minus => a - b,
        BinaryOperator::Multiply => a * b,
        BinaryOperator::Divide if b == 0.0 => {
            return Err(SqlError::Type("division by zero".into()));
        }
        BinaryOperator::Modulo if b == 0.0 => return Err(SqlError::Type("modulo by zero".into())),
        BinaryOperator::Divide => a / b,
        BinaryOperator::Modulo => a % b,
        other => return Err(SqlError::Unsupported(format!("operator: {other}"))),
    };
    Ok(Value::Double(out))
}

/// Convert a value to `target` (the `CAST` semantics). NULL passes through.
fn cast_value(v: Value, target: Type) -> Result<Value> {
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    let bad = |from: &Value, to: &str| SqlError::Type(format!("cannot cast {from:?} to {to}"));
    Ok(match target {
        Type::Bool => match v {
            Value::Bool(b) => Value::Bool(b),
            Value::Int64(n) => Value::Bool(n != 0),
            other => return Err(bad(&other, "BOOL")),
        },
        Type::Int64 => match v {
            Value::Int64(n) => Value::Int64(n),
            Value::Double(d) => Value::Int64(d as i64),
            Value::Bool(b) => Value::Int64(i64::from(b)),
            Value::Timestamp(t) => Value::Int64(t),
            Value::Text(ref s) => Value::Int64(s.trim().parse().map_err(|_| bad(&v, "BIGINT"))?),
            other => return Err(bad(&other, "BIGINT")),
        },
        Type::Double => match v {
            Value::Double(d) => Value::Double(d),
            Value::Int64(n) => Value::Double(n as f64),
            Value::Text(ref s) => Value::Double(s.trim().parse().map_err(|_| bad(&v, "DOUBLE"))?),
            other => return Err(bad(&other, "DOUBLE")),
        },
        Type::Timestamp => match v {
            Value::Timestamp(t) => Value::Timestamp(t),
            Value::Int64(n) => Value::Timestamp(n),
            Value::Text(ref s) => Value::Timestamp(types::parse_timestamp(s)?),
            other => return Err(bad(&other, "TIMESTAMP")),
        },
        Type::Text => Value::Text(v.display()),
    })
}

/// SQL `LIKE` matching with `%` (any run, including empty) and `_` (exactly one
/// character). Linear-time with greedy `%` backtracking.
fn like_match(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star_pi, mut star_ti): (Option<usize>, usize) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star_pi {
            // Backtrack: let the last `%` swallow one more character.
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
}

/// Resolve `ORDER BY` items to `(column index, ascending)` pairs. Only simple
/// column references are supported (ascending by default; `DESC` flips it).
/// Resolve an `ORDER BY` key for an aggregate/`GROUP BY` query to an output
/// column index: a 1-based ordinal, an output name, or the column's expression
/// text (so `ORDER BY COUNT(*)` matches the `COUNT(*)` output).
fn resolve_output_col(expr: &Expr, names: &[&str]) -> Result<usize> {
    if matches!(expr, Expr::Value(_)) {
        if let Value::Int64(n) = literal(expr)? {
            return usize::try_from(n)
                .ok()
                .filter(|&p| p >= 1 && p <= names.len())
                .map(|p| p - 1)
                .ok_or_else(|| SqlError::Type(format!("ORDER BY position {n} is out of range")));
        }
    }
    let key = match expr {
        Expr::Identifier(id) => id.value.clone(),
        Expr::CompoundIdentifier(parts) if !parts.is_empty() => parts.last().unwrap().value.clone(),
        other => other.to_string(),
    };
    names
        .iter()
        .position(|n| *n == key)
        .ok_or_else(|| SqlError::Unsupported(format!("ORDER BY {expr} is not an output column")))
}

fn resolve_order_keys(
    items: &[OrderByExpr],
    cols: &dyn ColumnResolver,
) -> Result<Vec<(usize, bool)>> {
    let mut keys = Vec::with_capacity(items.len());
    for item in items {
        // Supports `ORDER BY col` and `ORDER BY t.col`.
        let idx = resolve_col_expr(cols, &item.expr)?;
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
        (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
        // Numeric ordering across int/double (NaN sorts as Equal here).
        _ => match (as_f64(a), as_f64(b)) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            _ => Ordering::Equal,
        },
    }
}

/// The numeric value of an `Int64`/`Double`, for cross-type comparison and
/// arithmetic; `None` for non-numeric values.
fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int64(n) => Some(*n as f64),
        Value::Double(d) => Some(*d),
        _ => None,
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
    Avg,
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
            // SUM over numbers, skipping NULLs; an empty/all-NULL group is NULL.
            // An all-integer group sums to an integer; any double promotes the
            // running total (and result) to a double.
            AggFunc::Sum => {
                let c = self.col.expect("SUM has a column");
                let mut int_sum: i64 = 0;
                let mut float_sum: f64 = 0.0;
                let mut is_float = false;
                let mut seen = false;
                for &i in members {
                    match &rows[i][c] {
                        Value::Null => {}
                        Value::Int64(n) => {
                            if is_float {
                                float_sum += *n as f64;
                            } else {
                                int_sum += n;
                            }
                            seen = true;
                        }
                        Value::Double(d) => {
                            if !is_float {
                                is_float = true;
                                float_sum = int_sum as f64;
                            }
                            float_sum += d;
                            seen = true;
                        }
                        other => {
                            return Err(SqlError::Type(format!(
                                "SUM over a non-numeric value: {other:?}"
                            )));
                        }
                    }
                }
                Ok(match (seen, is_float) {
                    (false, _) => Value::Null,
                    (true, true) => Value::Double(float_sum),
                    (true, false) => Value::Int64(int_sum),
                })
            }
            // AVG over numbers, skipping NULLs; the result is always a double
            // (an empty/all-NULL group is NULL).
            AggFunc::Avg => {
                let c = self.col.expect("AVG has a column");
                let mut sum = 0.0;
                let mut count: i64 = 0;
                for &i in members {
                    match &rows[i][c] {
                        Value::Null => {}
                        v => match as_f64(v) {
                            Some(x) => {
                                sum += x;
                                count += 1;
                            }
                            None => {
                                return Err(SqlError::Type(format!(
                                    "AVG over a non-numeric value: {v:?}"
                                )));
                            }
                        },
                    }
                }
                Ok(if count == 0 {
                    Value::Null
                } else {
                    Value::Double(sum / count as f64)
                })
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

/// Whether `name` (case-insensitive) is an aggregate function.
fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "COUNT" | "SUM" | "MIN" | "MAX" | "AVG"
    )
}

/// Whether any projection item is an *aggregate* function call (scalar
/// functions like `YEAR`/`UPPER` are evaluated per row, not aggregated).
fn projection_has_aggregate(items: &[SelectItem]) -> bool {
    let is_agg =
        |e: &Expr| matches!(e, Expr::Function(f) if is_aggregate_name(&object_name(&f.name)));
    items.iter().any(|item| match item {
        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => is_agg(e),
        _ => false,
    })
}

/// Parse a `GROUP BY` clause to a list of distinct key column indices. Only
/// simple column references are supported.
fn parse_group_by(group_by: &GroupByExpr, cols: &dyn ColumnResolver) -> Result<Vec<usize>> {
    match group_by {
        GroupByExpr::Expressions(exprs, modifiers) => {
            if !modifiers.is_empty() {
                return Err(SqlError::Unsupported("GROUP BY modifiers".into()));
            }
            let mut keys = Vec::with_capacity(exprs.len());
            for e in exprs {
                let idx = resolve_col_expr(cols, e)?;
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
fn parse_aggregate(f: &Function, cols: &dyn ColumnResolver) -> Result<Aggregate> {
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
    // Resolve a column argument (`col` or `t.col`) to its index.
    let col_of = |arg: &FunctionArgExpr| -> Result<usize> {
        match arg {
            FunctionArgExpr::Expr(e) => resolve_col_expr(cols, e),
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
        "AVG" => AggFunc::Avg,
        "MIN" => AggFunc::Min,
        "MAX" => AggFunc::Max,
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

/// Bytes appended to a non-unique secondary-index key to make each row's entry
/// unique: the record id's page (u64 BE) then slot (u16 BE).
const RID_SUFFIX_LEN: usize = 10;

fn rid_suffix(rid: RecordId) -> [u8; RID_SUFFIX_LEN] {
    let mut s = [0u8; RID_SUFFIX_LEN];
    s[..8].copy_from_slice(&rid.page.as_u64().to_be_bytes());
    s[8..].copy_from_slice(&rid.slot.to_be_bytes());
    s
}

/// A non-unique index key: the composite value key followed by the row id.
fn nonunique_key(framed: &[u8], rid: RecordId) -> Vec<u8> {
    let mut k = Vec::with_capacity(framed.len() + RID_SUFFIX_LEN);
    k.extend_from_slice(framed);
    k.extend_from_slice(&rid_suffix(rid));
    k
}

/// Frame (non-NULL) values into a self-delimiting composite key — each part is a
/// `u32` length followed by its order-preserving bytes — so a multi-column key is
/// unambiguous and all entries for one value share a prefix.
fn frame_index_key(values: &[&Value]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for v in values {
        let part = encode_index_key(v)?;
        out.extend_from_slice(&(part.len() as u32).to_le_bytes());
        out.extend_from_slice(&part);
    }
    Ok(out)
}

/// The composite key for `row`'s `columns`, or `None` if any indexed column is
/// NULL (rows with a NULL in an indexed column are not indexed).
fn index_row_key(row: &[Value], columns: &[usize]) -> Result<Option<Vec<u8>>> {
    if columns.iter().any(|&c| matches!(row[c], Value::Null)) {
        return Ok(None);
    }
    let vals: Vec<&Value> = columns.iter().map(|&c| &row[c]).collect();
    Ok(Some(frame_index_key(&vals)?))
}

/// Encode a value as an order-preserving B+tree index key. Integers use a
/// sign-flipped big-endian form so the byte order matches numeric order.
fn encode_index_key(value: &Value) -> Result<Vec<u8>> {
    Ok(match value {
        Value::Int64(n) | Value::Timestamp(n) => (*n as u64 ^ (1u64 << 63)).to_be_bytes().to_vec(),
        // Order-preserving float key: flip the sign bit for positives, all bits
        // for negatives, so the big-endian bytes sort in numeric order.
        Value::Double(d) => {
            let bits = d.to_bits();
            let ordered = if bits & (1u64 << 63) != 0 {
                !bits
            } else {
                bits ^ (1u64 << 63)
            };
            ordered.to_be_bytes().to_vec()
        }
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
            Value::Double(d) => Ok(Value::Double(-d)),
            other => Err(SqlError::Type(format!("cannot negate {other:?}"))),
        },
        other => Err(SqlError::Unsupported(format!("literal: {other:?}"))),
    }
}

fn sql_value(v: &SqlValue) -> Result<Value> {
    match v {
        // A bare integer stays an integer; a literal with a decimal point or
        // exponent (e.g. `9.5`, `1e3`) is a double.
        SqlValue::Number(n, _) => {
            if n.contains(['.', 'e', 'E']) {
                n.parse::<f64>()
                    .map(Value::Double)
                    .map_err(|_| SqlError::Type(format!("not a number: {n}")))
            } else {
                n.parse::<i64>()
                    .map(Value::Int64)
                    .map_err(|_| SqlError::Type(format!("not an integer: {n}")))
            }
        }
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
        DataType::Float(_) | DataType::Real | DataType::Double(_) | DataType::DoublePrecision => {
            Ok(Type::Double)
        }
        DataType::Timestamp(_, _) | DataType::Datetime(_) | DataType::Date => Ok(Type::Timestamp),
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
    fn drop_table_removes_it_and_frees_the_name() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,'a')")
            .unwrap();

        assert_eq!(
            env.engine.execute_autocommit("DROP TABLE t").unwrap(),
            Outcome::DropTable { name: "t".into() }
        );
        // The table is gone: queries against it fail.
        assert!(matches!(
            env.engine.execute_autocommit("SELECT id FROM t"),
            Err(SqlError::NoSuchTable(_))
        ));
        // The name is free to reuse — with a fresh, independent schema.
        env.engine
            .execute_autocommit("CREATE TABLE t (other BIGINT)")
            .unwrap();
        assert_eq!(
            env.engine
                .execute_autocommit("SELECT other FROM t")
                .unwrap(),
            Outcome::Select {
                columns: vec!["other".into()],
                rows: vec![],
            }
        );
    }

    #[test]
    fn drop_missing_table_errors_unless_if_exists() {
        let env = env();
        assert!(matches!(
            env.engine.execute_autocommit("DROP TABLE ghost"),
            Err(SqlError::NoSuchTable(_))
        ));
        // IF EXISTS makes it a no-op.
        assert_eq!(
            env.engine
                .execute_autocommit("DROP TABLE IF EXISTS ghost")
                .unwrap(),
            Outcome::DropTable {
                name: "ghost".into()
            }
        );
    }

    #[test]
    fn alter_add_column_backfills_null_and_keeps_index() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,'alice'),(2,'bob')")
            .unwrap();

        assert_eq!(
            env.engine
                .execute_autocommit("ALTER TABLE t ADD COLUMN age BIGINT")
                .unwrap(),
            Outcome::AlterTable {
                table: "t".into(),
                renamed_from: None,
            }
        );
        // An old row, fetched via the primary-key index, carries NULL for the
        // new column (proves the row was re-encoded and the index repointed).
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT age FROM t WHERE id = 1")
                    .unwrap()
            ),
            vec![vec![Value::Null]]
        );
        // New inserts populate the column.
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (3,'carol',41)")
            .unwrap();
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT age FROM t WHERE id = 3")
                    .unwrap()
            ),
            vec![vec![Value::Int64(41)]]
        );
    }

    #[test]
    fn alter_drop_column_reencodes_rows() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,'alice',30),(2,'bob',25)")
            .unwrap();

        env.engine
            .execute_autocommit("ALTER TABLE t DROP COLUMN name")
            .unwrap();
        // Remaining columns decode correctly and the index still seeks.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT id, age FROM t WHERE id = 2")
                    .unwrap()
            ),
            vec![vec![Value::Int64(2), Value::Int64(25)]]
        );
        // The dropped column is gone; the PRIMARY KEY column cannot be dropped.
        assert!(matches!(
            env.engine.execute_autocommit("SELECT name FROM t"),
            Err(SqlError::NoSuchColumn(_))
        ));
        assert!(
            env.engine
                .execute_autocommit("ALTER TABLE t DROP COLUMN id")
                .is_err()
        );
    }

    #[test]
    fn alter_rename_column_and_table() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,'alice')")
            .unwrap();

        env.engine
            .execute_autocommit("ALTER TABLE t RENAME COLUMN name TO full_name")
            .unwrap();
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT full_name FROM t")
                    .unwrap()
            ),
            vec![vec![Value::Text("alice".into())]]
        );
        assert!(matches!(
            env.engine.execute_autocommit("SELECT name FROM t"),
            Err(SqlError::NoSuchColumn(_))
        ));

        assert_eq!(
            env.engine
                .execute_autocommit("ALTER TABLE t RENAME TO people")
                .unwrap(),
            Outcome::AlterTable {
                table: "people".into(),
                renamed_from: Some("t".into()),
            }
        );
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT id FROM people")
                    .unwrap()
            ),
            vec![vec![Value::Int64(1)]]
        );
        assert!(matches!(
            env.engine.execute_autocommit("SELECT id FROM t"),
            Err(SqlError::NoSuchTable(_))
        ));
    }

    #[test]
    fn alter_add_not_null_column_requires_empty_table() {
        let nonempty = env();
        nonempty
            .engine
            .execute_autocommit("CREATE TABLE t (id BIGINT)")
            .unwrap();
        nonempty
            .engine
            .execute_autocommit("INSERT INTO t VALUES (1)")
            .unwrap();
        // A non-empty table has no value for a new NOT NULL column.
        assert!(
            nonempty
                .engine
                .execute_autocommit("ALTER TABLE t ADD COLUMN x BIGINT NOT NULL")
                .is_err()
        );
        // On an empty table it is allowed.
        let empty = env();
        empty
            .engine
            .execute_autocommit("CREATE TABLE t (id BIGINT)")
            .unwrap();
        empty
            .engine
            .execute_autocommit("ALTER TABLE t ADD COLUMN x BIGINT NOT NULL")
            .unwrap();
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
        // AVG now yields a double.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT AVG(v) FROM t")
                    .unwrap()
            ),
            vec![vec![Value::Double(10.0)]]
        );
    }

    fn seed_ops(env: &Env) {
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT, name TEXT, score BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit(
                "INSERT INTO t VALUES \
                 (1,'alice',10),(2,'bob',NULL),(3,'carol',30),(4,'dave',40)",
            )
            .unwrap();
    }

    #[test]
    fn arithmetic_in_projection_and_predicate() {
        let env = env();
        seed_ops(&env);

        // Expression in the select list, with an alias.
        let out = env
            .engine
            .execute_autocommit("SELECT id, id * 100 AS scaled FROM t WHERE id = 3")
            .unwrap();
        match out {
            Outcome::Select { columns, rows } => {
                assert_eq!(columns, vec!["id", "scaled"]);
                assert_eq!(rows, vec![vec![Value::Int64(3), Value::Int64(300)]]);
            }
            other => panic!("{other:?}"),
        }

        // Arithmetic inside the predicate.
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t WHERE id + 1 > 3 ORDER BY id")
                    .unwrap()
            ),
            vec![3, 4]
        );

        // Derived column name when there is no alias.
        let out = env
            .engine
            .execute_autocommit("SELECT id % 2 FROM t WHERE id = 1")
            .unwrap();
        if let Outcome::Select { columns, .. } = out {
            assert_eq!(columns.len(), 1);
            assert!(columns[0].contains('%'), "name was {:?}", columns[0]);
        }
    }

    #[test]
    fn division_by_zero_is_an_error() {
        let env = env();
        seed_ops(&env);
        assert!(matches!(
            env.engine.execute_autocommit("SELECT id / 0 FROM t"),
            Err(SqlError::Type(_))
        ));
    }

    #[test]
    fn update_set_supports_arithmetic() {
        let env = env();
        seed_ops(&env);
        // score: 10, NULL, 30, 40 -> +5 each (NULL stays NULL).
        env.engine
            .execute_autocommit("UPDATE t SET score = score + 5")
            .unwrap();
        let r = rows(
            env.engine
                .execute_autocommit("SELECT id, score FROM t ORDER BY id")
                .unwrap(),
        );
        assert_eq!(
            r,
            vec![
                vec![Value::Int64(1), Value::Int64(15)],
                vec![Value::Int64(2), Value::Null],
                vec![Value::Int64(3), Value::Int64(35)],
                vec![Value::Int64(4), Value::Int64(45)],
            ]
        );
    }

    #[test]
    fn is_null_and_is_not_null() {
        let env = env();
        seed_ops(&env);
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t WHERE score IS NULL")
                    .unwrap()
            ),
            vec![2]
        );
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t WHERE score IS NOT NULL ORDER BY id")
                    .unwrap()
            ),
            vec![1, 3, 4]
        );
    }

    #[test]
    fn in_list_and_not_in() {
        let env = env();
        seed_ops(&env);
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t WHERE id IN (1, 3) ORDER BY id")
                    .unwrap()
            ),
            vec![1, 3]
        );
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit(
                        "SELECT id FROM t WHERE name NOT IN ('alice','bob') ORDER BY id"
                    )
                    .unwrap()
            ),
            vec![3, 4]
        );
    }

    #[test]
    fn between_and_not_between() {
        let env = env();
        seed_ops(&env);
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t WHERE id BETWEEN 2 AND 3 ORDER BY id")
                    .unwrap()
            ),
            vec![2, 3]
        );
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t WHERE id NOT BETWEEN 2 AND 3 ORDER BY id")
                    .unwrap()
            ),
            vec![1, 4]
        );
    }

    #[test]
    fn like_patterns() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,'alice'),(2,'alan'),(3,'bob'),(4,'al')")
            .unwrap();

        // Prefix wildcard: names starting with "al".
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t WHERE name LIKE 'al%' ORDER BY id")
                    .unwrap()
            ),
            vec![1, 2, 4]
        );
        // `_` matches exactly one character: '___' matches only 3-char names.
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t WHERE name LIKE '___'")
                    .unwrap()
            ),
            vec![3] // 'bob'
        );
        // Mixed: starts 'a', ends 'e'.
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t WHERE name LIKE 'a%e'")
                    .unwrap()
            ),
            vec![1] // 'alice'
        );
        // NOT LIKE.
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t WHERE name NOT LIKE 'al%' ORDER BY id")
                    .unwrap()
            ),
            vec![3]
        );
    }

    #[test]
    fn not_operator_negates_a_predicate() {
        let env = env();
        seed_ops(&env);
        assert_eq!(
            ints(
                env.engine
                    .execute_autocommit("SELECT id FROM t WHERE NOT (id = 2) ORDER BY id")
                    .unwrap()
            ),
            vec![1, 3, 4]
        );
    }

    #[test]
    fn select_distinct_dedupes_rows() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT, city TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit(
                "INSERT INTO t VALUES (1,'NYC'),(2,'LA'),(3,'NYC'),(4,'LA'),(5,'SF')",
            )
            .unwrap();

        let r = rows(
            env.engine
                .execute_autocommit("SELECT DISTINCT city FROM t ORDER BY city")
                .unwrap(),
        );
        assert_eq!(
            r,
            vec![
                vec![Value::Text("LA".into())],
                vec![Value::Text("NYC".into())],
                vec![Value::Text("SF".into())],
            ]
        );

        // Without DISTINCT all five rows come back.
        let all = rows(env.engine.execute_autocommit("SELECT city FROM t").unwrap());
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn select_distinct_on_multiple_columns() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (a BIGINT, b BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,1),(1,1),(1,2),(2,1)")
            .unwrap();
        let r = rows(
            env.engine
                .execute_autocommit("SELECT DISTINCT a, b FROM t ORDER BY a, b")
                .unwrap(),
        );
        assert_eq!(
            r.len(),
            3,
            "(1,1),(1,2),(2,1) — the duplicate (1,1) collapses"
        );
    }

    #[test]
    fn having_filters_groups() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (g BIGINT, v BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,10),(1,20),(2,5),(3,100),(3,1),(3,9)")
            .unwrap();

        // Groups with more than one row: g=1 (2 rows) and g=3 (3 rows).
        let r = rows(
            env.engine
                .execute_autocommit("SELECT g, COUNT(*) FROM t GROUP BY g HAVING COUNT(*) > 1")
                .unwrap(),
        );
        assert_eq!(
            r,
            vec![
                vec![Value::Int64(1), Value::Int64(2)],
                vec![Value::Int64(3), Value::Int64(3)],
            ]
        );

        // HAVING on SUM, combined with a group-key condition.
        let r = rows(
            env.engine
                .execute_autocommit(
                    "SELECT g, SUM(v) FROM t GROUP BY g HAVING SUM(v) >= 100 AND g > 1",
                )
                .unwrap(),
        );
        assert_eq!(r, vec![vec![Value::Int64(3), Value::Int64(110)]]);
    }

    #[test]
    fn having_without_group_by_filters_the_whole_table() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (v BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1),(2),(3)")
            .unwrap();
        // The single implicit group passes (COUNT = 3 > 2).
        let r = rows(
            env.engine
                .execute_autocommit("SELECT COUNT(*) FROM t HAVING COUNT(*) > 2")
                .unwrap(),
        );
        assert_eq!(r, vec![vec![Value::Int64(3)]]);
        // A failing HAVING yields no rows.
        let r = rows(
            env.engine
                .execute_autocommit("SELECT COUNT(*) FROM t HAVING COUNT(*) > 5")
                .unwrap(),
        );
        assert!(r.is_empty());
    }

    #[test]
    fn date_functions_extract_components() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT PRIMARY KEY, ts BIGINT)")
            .unwrap();
        // 1609462930 = 2021-01-01 01:02:10 UTC.
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1, 1609462930)")
            .unwrap();
        let r = rows(
            env.engine
                .execute_autocommit(
                    "SELECT YEAR(ts), MONTH(ts), DAY(ts), HOUR(ts), MINUTE(ts), SECOND(ts) FROM t",
                )
                .unwrap(),
        );
        assert_eq!(
            r[0],
            vec![
                Value::Int64(2021),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(2),
                Value::Int64(10),
            ]
        );
    }

    #[test]
    fn date_functions_usable_in_where_and_with_now() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT PRIMARY KEY, ts BIGINT)")
            .unwrap();
        // 2021-06-15, 2022-01-01, 2021-12-31 (approx epochs).
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,1623715200),(2,1640995200),(3,1640908800)")
            .unwrap();
        let r = ints(
            env.engine
                .execute_autocommit("SELECT id FROM t WHERE YEAR(ts) = 2021 ORDER BY id")
                .unwrap(),
        );
        assert_eq!(r, vec![1, 3]);

        // NOW() is a recent epoch (after 2020-01-01).
        let now = ints(
            env.engine
                .execute_autocommit("SELECT NOW() FROM t")
                .unwrap(),
        );
        assert!(now[0] > 1_577_836_800, "NOW() should be after 2020");
    }

    #[test]
    fn string_and_numeric_functions() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1, '  Héllo  ')")
            .unwrap();
        let r = rows(
            env.engine
                .execute_autocommit(
                    "SELECT UPPER(name), LOWER('AB'), LENGTH(TRIM(name)), SUBSTR('hello',2,3), \
                     CONCAT('a','b',id), ABS(0-7), MOD(10,3), COALESCE(NULL, 'x') FROM t",
                )
                .unwrap(),
        );
        assert_eq!(
            r[0],
            vec![
                Value::Text("  HÉLLO  ".into()),
                Value::Text("ab".into()),
                Value::Int64(5), // "Héllo" is 5 chars
                Value::Text("ell".into()),
                Value::Text("ab1".into()),
                Value::Int64(7),
                Value::Int64(1),
                Value::Text("x".into()),
            ]
        );
    }

    // ---- joins ----------------------------------------------------------

    fn seed_join(env: &Env) {
        env.engine
            .execute_autocommit("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit(
                "CREATE TABLE orders (id BIGINT PRIMARY KEY, user_id BIGINT, total BIGINT)",
            )
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO users VALUES (1,'alice'),(2,'bob'),(3,'carol')")
            .unwrap();
        // alice: two orders, bob: one, carol: none; order 13 references no user.
        env.engine
            .execute_autocommit(
                "INSERT INTO orders VALUES (10,1,100),(11,1,50),(12,2,70),(13,99,5)",
            )
            .unwrap();
    }

    fn t(s: &str) -> Value {
        Value::Text(s.into())
    }

    #[test]
    fn inner_join_matches_on_key() {
        let env = env();
        seed_join(&env);
        let r = rows(
            env.engine
                .execute_autocommit(
                    "SELECT u.name, o.total FROM users u JOIN orders o \
                     ON u.id = o.user_id ORDER BY o.total",
                )
                .unwrap(),
        );
        assert_eq!(
            r,
            vec![
                vec![t("alice"), Value::Int64(50)],
                vec![t("bob"), Value::Int64(70)],
                vec![t("alice"), Value::Int64(100)],
            ]
        );
    }

    #[test]
    fn left_join_keeps_unmatched_left_with_nulls() {
        let env = env();
        seed_join(&env);
        let r = rows(
            env.engine
                .execute_autocommit(
                    "SELECT u.name, o.total FROM users u LEFT JOIN orders o \
                     ON u.id = o.user_id ORDER BY u.name, o.total",
                )
                .unwrap(),
        );
        assert_eq!(
            r,
            vec![
                vec![t("alice"), Value::Int64(50)],
                vec![t("alice"), Value::Int64(100)],
                vec![t("bob"), Value::Int64(70)],
                vec![t("carol"), Value::Null],
            ]
        );
    }

    #[test]
    fn right_join_keeps_unmatched_right_with_nulls() {
        let env = env();
        seed_join(&env);
        let r = rows(
            env.engine
                .execute_autocommit(
                    "SELECT u.name, o.total FROM users u RIGHT JOIN orders o \
                     ON u.id = o.user_id ORDER BY o.total",
                )
                .unwrap(),
        );
        assert_eq!(
            r,
            vec![
                vec![Value::Null, Value::Int64(5)], // order 13: no matching user
                vec![t("alice"), Value::Int64(50)],
                vec![t("bob"), Value::Int64(70)],
                vec![t("alice"), Value::Int64(100)],
            ]
        );
    }

    #[test]
    fn full_join_keeps_both_unmatched_sides() {
        let env = env();
        seed_join(&env);
        let r = rows(
            env.engine
                .execute_autocommit(
                    "SELECT u.name, o.total FROM users u FULL JOIN orders o ON u.id = o.user_id",
                )
                .unwrap(),
        );
        assert_eq!(r.len(), 5); // 3 matched + carol (no order) + order 13 (no user)
        assert!(r.contains(&vec![t("carol"), Value::Null]));
        assert!(r.contains(&vec![Value::Null, Value::Int64(5)]));
    }

    #[test]
    fn cross_join_is_the_cartesian_product() {
        let env = env();
        seed_join(&env);
        let comma = rows(
            env.engine
                .execute_autocommit("SELECT u.id, o.id FROM users u, orders o")
                .unwrap(),
        );
        let keyword = rows(
            env.engine
                .execute_autocommit("SELECT u.id FROM users u CROSS JOIN orders o")
                .unwrap(),
        );
        assert_eq!(comma.len(), 12); // 3 users × 4 orders
        assert_eq!(keyword.len(), 12);
    }

    #[test]
    fn self_join_resolves_through_aliases() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE emp (id BIGINT PRIMARY KEY, name TEXT, mgr BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO emp VALUES (1,'ceo',NULL),(2,'alice',1),(3,'bob',1)")
            .unwrap();
        let r = rows(
            env.engine
                .execute_autocommit(
                    "SELECT e.name, m.name FROM emp e JOIN emp m ON e.mgr = m.id ORDER BY e.name",
                )
                .unwrap(),
        );
        assert_eq!(
            r,
            vec![vec![t("alice"), t("ceo")], vec![t("bob"), t("ceo")]]
        );
    }

    #[test]
    fn join_with_where_and_aggregate() {
        let env = env();
        seed_join(&env);
        // GROUP BY over a join: total spend per user that has orders.
        let r = rows(
            env.engine
                .execute_autocommit(
                    "SELECT u.name, SUM(o.total) FROM users u JOIN orders o \
                     ON u.id = o.user_id GROUP BY u.name",
                )
                .unwrap(),
        );
        assert_eq!(
            r,
            vec![
                vec![t("alice"), Value::Int64(150)],
                vec![t("bob"), Value::Int64(70)],
            ]
        );

        // WHERE over a joined row.
        let r2 = rows(
            env.engine
                .execute_autocommit(
                    "SELECT o.total FROM users u JOIN orders o ON u.id = o.user_id \
                     WHERE u.name = 'alice' ORDER BY o.total",
                )
                .unwrap(),
        );
        assert_eq!(r2, vec![vec![Value::Int64(50)], vec![Value::Int64(100)]]);
    }

    #[test]
    fn star_over_a_join_expands_all_columns() {
        let env = env();
        seed_join(&env);
        let out = env
            .engine
            .execute_autocommit("SELECT * FROM users u JOIN orders o ON u.id = o.user_id")
            .unwrap();
        match out {
            Outcome::Select { columns, .. } => assert_eq!(columns.len(), 5), // id,name,id,user_id,total
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ambiguous_unqualified_column_is_rejected() {
        let env = env();
        seed_join(&env);
        // `id` exists in both tables: a bare reference must error.
        assert!(
            env.engine
                .execute_autocommit("SELECT id FROM users u JOIN orders o ON u.id = o.user_id")
                .is_err()
        );
    }

    #[test]
    fn control_flow_and_scalar_functions() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (5, 'hello')")
            .unwrap();
        let r = rows(
            env.engine
                .execute_autocommit(
                    "SELECT \
                     CASE WHEN id > 3 THEN 'big' ELSE 'small' END, \
                     CASE id WHEN 5 THEN 'five' ELSE 'other' END, \
                     IF(id > 3, 'yes', 'no'), \
                     IFNULL(NULL, 'fallback'), \
                     NULLIF(id, 5), \
                     REPLACE(name, 'l', 'L'), \
                     ROUND(1234, -2), \
                     CEIL(7), FLOOR(7), \
                     POW(2, 10), \
                     DATEDIFF(172800, 86400), \
                     DATE_ADD(0, INTERVAL 2 DAY), \
                     DATE_SUB(172800, INTERVAL 1 DAY) \
                     FROM t",
                )
                .unwrap(),
        );
        assert_eq!(
            r[0],
            vec![
                t("big"),
                t("five"),
                t("yes"),
                t("fallback"),
                Value::Null,
                t("heLLo"),
                Value::Int64(1200),
                Value::Int64(7),
                Value::Int64(7),
                Value::Int64(1024),
                Value::Int64(1),
                Value::Int64(172_800),
                Value::Int64(86_400),
            ]
        );
    }

    #[test]
    fn double_type_aggregates_and_arithmetic() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE n (id BIGINT, x BIGINT, d DOUBLE)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO n VALUES (1, 10, 2.5), (2, 20, 7.5), (3, 30, NULL)")
            .unwrap();

        // AVG always yields a double (over an int column too); SUM keeps the
        // column's type; NULLs are skipped.
        let agg = rows(
            env.engine
                .execute_autocommit("SELECT AVG(x), AVG(d), SUM(d), SUM(x), MIN(d), MAX(d) FROM n")
                .unwrap(),
        );
        assert_eq!(
            agg[0],
            vec![
                Value::Double(20.0),
                Value::Double(5.0),
                Value::Double(10.0),
                Value::Int64(60),
                Value::Double(2.5),
                Value::Double(7.5),
            ]
        );

        // double * int -> double; ORDER BY a double; WHERE double > int literal.
        let q = rows(
            env.engine
                .execute_autocommit("SELECT d * 2 FROM n WHERE d > 3 ORDER BY d")
                .unwrap(),
        );
        assert_eq!(q, vec![vec![Value::Double(15.0)]]);

        // An integer literal widens into a DOUBLE column on insert.
        env.engine
            .execute_autocommit("INSERT INTO n VALUES (4, 40, 8)")
            .unwrap();
        let coerced = rows(
            env.engine
                .execute_autocommit("SELECT d FROM n WHERE id = 4")
                .unwrap(),
        );
        assert_eq!(coerced, vec![vec![Value::Double(8.0)]]);

        // Float scalar functions.
        let f = rows(
            env.engine
                .execute_autocommit(
                    "SELECT ROUND(2.5, 0), CEIL(1.2), FLOOR(1.8), ABS(-3.5) FROM n WHERE id = 1",
                )
                .unwrap(),
        );
        assert_eq!(
            f[0],
            vec![
                Value::Double(3.0),
                Value::Double(2.0),
                Value::Double(1.0),
                Value::Double(3.5),
            ]
        );
    }

    #[test]
    fn order_by_over_aggregate_output() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE s (g TEXT, v BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO s VALUES ('a',1),('b',5),('a',2),('c',3)")
            .unwrap();
        // Order groups by the aggregate alias, descending (ties keep group order).
        let by_alias = rows(
            env.engine
                .execute_autocommit("SELECT g, SUM(v) total FROM s GROUP BY g ORDER BY total DESC")
                .unwrap(),
        );
        assert_eq!(
            by_alias,
            vec![
                vec![t("b"), Value::Int64(5)],
                vec![t("a"), Value::Int64(3)],
                vec![t("c"), Value::Int64(3)],
            ]
        );
        // The same, by 1-based output ordinal.
        let by_ordinal = rows(
            env.engine
                .execute_autocommit("SELECT g, SUM(v) FROM s GROUP BY g ORDER BY 2 DESC")
                .unwrap(),
        );
        assert_eq!(by_ordinal, by_alias);
    }

    #[test]
    fn timestamp_type_parse_extract_and_cast() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE e (id BIGINT, at TIMESTAMP)")
            .unwrap();
        // A string in a TIMESTAMP column is parsed (date, or date + time).
        env.engine
            .execute_autocommit(
                "INSERT INTO e VALUES (1, '2021-06-15 12:30:00'), (2, '2020-01-02'), \
                 (3, '2022-12-31 23:59:59')",
            )
            .unwrap();

        // YEAR/MONTH/DAY extract over a TIMESTAMP column.
        let parts = rows(
            env.engine
                .execute_autocommit("SELECT YEAR(at), MONTH(at), DAY(at) FROM e WHERE id = 1")
                .unwrap(),
        );
        assert_eq!(
            parts[0],
            vec![Value::Int64(2021), Value::Int64(6), Value::Int64(15)]
        );

        // WHERE against a CAST string literal, and ORDER BY a timestamp.
        let after = rows(
            env.engine
                .execute_autocommit(
                    "SELECT id FROM e WHERE at >= CAST('2021-01-01' AS TIMESTAMP) ORDER BY at",
                )
                .unwrap(),
        );
        assert_eq!(after, vec![vec![Value::Int64(1)], vec![Value::Int64(3)]]);

        // CAST a timestamp to text yields the canonical display form.
        let text = rows(
            env.engine
                .execute_autocommit("SELECT CAST(at AS TEXT) FROM e WHERE id = 2")
                .unwrap(),
        );
        assert_eq!(
            text[0],
            vec![Value::Text("2020-01-02 00:00:00".to_string())]
        );
    }

    #[test]
    fn using_and_natural_joins_coalesce_columns() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE l (id BIGINT, lname TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("CREATE TABLE r (id BIGINT, rname TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO l VALUES (1,'a'),(2,'b'),(3,'c')")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO r VALUES (2,'x'),(3,'y'),(4,'z')")
            .unwrap();

        // USING(id): the join column appears once; bare `id` is unambiguous.
        let u = rows(
            env.engine
                .execute_autocommit("SELECT id, lname, rname FROM l JOIN r USING (id) ORDER BY id")
                .unwrap(),
        );
        assert_eq!(
            u,
            vec![
                vec![Value::Int64(2), t("b"), t("x")],
                vec![Value::Int64(3), t("c"), t("y")],
            ]
        );

        // SELECT * shows the coalesced column once: id, then left, then right.
        match env
            .engine
            .execute_autocommit("SELECT * FROM l JOIN r USING (id)")
            .unwrap()
        {
            Outcome::Select { columns, .. } => assert_eq!(columns, vec!["id", "lname", "rname"]),
            other => panic!("{other:?}"),
        }

        // LEFT JOIN USING keeps the left id (coalesced), right side NULL.
        let lj = rows(
            env.engine
                .execute_autocommit("SELECT id, rname FROM l LEFT JOIN r USING (id) ORDER BY id")
                .unwrap(),
        );
        assert_eq!(
            lj,
            vec![
                vec![Value::Int64(1), Value::Null],
                vec![Value::Int64(2), t("x")],
                vec![Value::Int64(3), t("y")],
            ]
        );

        // RIGHT JOIN USING coalesces id from the right when the left is missing.
        let rj = rows(
            env.engine
                .execute_autocommit("SELECT id, lname FROM l RIGHT JOIN r USING (id) ORDER BY id")
                .unwrap(),
        );
        assert_eq!(
            rj,
            vec![
                vec![Value::Int64(2), t("b")],
                vec![Value::Int64(3), t("c")],
                vec![Value::Int64(4), Value::Null],
            ]
        );

        // NATURAL JOIN uses all common columns (here, just `id`).
        let nat = rows(
            env.engine
                .execute_autocommit("SELECT id FROM l NATURAL JOIN r ORDER BY id")
                .unwrap(),
        );
        assert_eq!(nat, vec![vec![Value::Int64(2)], vec![Value::Int64(3)]]);
    }

    #[test]
    fn unique_secondary_index_enforces_and_drops() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE u (id BIGINT PRIMARY KEY, email TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO u VALUES (1,'a@x'),(2,'b@x')")
            .unwrap();
        env.engine
            .execute_autocommit("CREATE UNIQUE INDEX u_email ON u (email)")
            .unwrap();

        // A duplicate value on the indexed column is rejected on INSERT.
        assert!(
            env.engine
                .execute_autocommit("INSERT INTO u VALUES (3,'a@x')")
                .is_err()
        );
        assert_eq!(
            affected(
                env.engine
                    .execute_autocommit("INSERT INTO u VALUES (3,'c@x')")
                    .unwrap()
            ),
            1
        );

        // UPDATE into an existing value is rejected; into a fresh value is fine.
        assert!(
            env.engine
                .execute_autocommit("UPDATE u SET email = 'b@x' WHERE id = 1")
                .is_err()
        );
        assert_eq!(
            affected(
                env.engine
                    .execute_autocommit("UPDATE u SET email = 'z@x' WHERE id = 1")
                    .unwrap()
            ),
            1
        );

        // Building a UNIQUE index over duplicate data fails.
        env.engine
            .execute_autocommit("CREATE TABLE d (id BIGINT, k BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO d VALUES (1,5),(2,5)")
            .unwrap();
        assert!(
            env.engine
                .execute_autocommit("CREATE UNIQUE INDEX d_k ON d (k)")
                .is_err()
        );

        // After DROP INDEX the constraint is gone, so a duplicate is accepted.
        env.engine.execute_autocommit("DROP INDEX u_email").unwrap();
        assert_eq!(
            affected(
                env.engine
                    .execute_autocommit("INSERT INTO u VALUES (4,'c@x')")
                    .unwrap()
            ),
            1
        );
    }

    #[test]
    fn subqueries_scalar_in_exists_and_derived() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT, v BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,10),(2,20),(3,30)")
            .unwrap();
        env.engine
            .execute_autocommit("CREATE TABLE big (id BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO big VALUES (2),(3)")
            .unwrap();

        // Scalar subquery in WHERE.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT id FROM t WHERE v = (SELECT MAX(v) FROM t)")
                    .unwrap()
            ),
            vec![vec![Value::Int64(3)]]
        );

        // Scalar subquery in the projection.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT id, (SELECT COUNT(*) FROM big) FROM t WHERE id = 1")
                    .unwrap()
            )[0],
            vec![Value::Int64(1), Value::Int64(2)]
        );

        // IN / NOT IN (subquery).
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit(
                        "SELECT id FROM t WHERE id IN (SELECT id FROM big) ORDER BY id"
                    )
                    .unwrap()
            ),
            vec![vec![Value::Int64(2)], vec![Value::Int64(3)]]
        );
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit(
                        "SELECT id FROM t WHERE id NOT IN (SELECT id FROM big) ORDER BY id"
                    )
                    .unwrap()
            ),
            vec![vec![Value::Int64(1)]]
        );

        // EXISTS / NOT EXISTS.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT id FROM t WHERE EXISTS (SELECT 1 FROM big)")
                    .unwrap()
            )
            .len(),
            3
        );
        env.engine
            .execute_autocommit("CREATE TABLE empty (id BIGINT)")
            .unwrap();
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM empty)")
                    .unwrap()
            )
            .len(),
            3
        );

        // Derived table in FROM.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit(
                        "SELECT d.v FROM (SELECT v FROM t WHERE v >= 20) d ORDER BY d.v"
                    )
                    .unwrap()
            ),
            vec![vec![Value::Int64(20)], vec![Value::Int64(30)]]
        );

        // Derived table joined to a base table.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit(
                        "SELECT t.id, m.mx FROM t JOIN (SELECT MAX(v) mx FROM t) m ON t.v = m.mx"
                    )
                    .unwrap()
            ),
            vec![vec![Value::Int64(3), Value::Int64(30)]]
        );
    }

    #[test]
    fn index_seek_returns_correct_rows() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE u (id BIGINT PRIMARY KEY, email TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO u VALUES (1,'a@x'),(2,'b@x'),(3,'c@x')")
            .unwrap();
        env.engine
            .execute_autocommit("CREATE UNIQUE INDEX u_email ON u (email)")
            .unwrap();

        // Seek via the secondary UNIQUE index.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT id FROM u WHERE email = 'b@x'")
                    .unwrap()
            ),
            vec![vec![Value::Int64(2)]]
        );
        // A residual predicate is still applied to the seeked row.
        assert!(
            rows(
                env.engine
                    .execute_autocommit("SELECT id FROM u WHERE email = 'b@x' AND id < 0")
                    .unwrap()
            )
            .is_empty()
        );
        // No match yields no rows.
        assert!(
            rows(
                env.engine
                    .execute_autocommit("SELECT id FROM u WHERE email = 'z@x'")
                    .unwrap()
            )
            .is_empty()
        );
        // The primary-key seek still works (with ORDER BY/LIMIT honored).
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT email FROM u WHERE id = 3 LIMIT 1")
                    .unwrap()
            ),
            vec![vec![t("c@x")]]
        );
    }

    #[test]
    fn nonunique_and_multicolumn_indexes() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE p (id BIGINT, city TEXT, age BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit(
                "INSERT INTO p VALUES (1,'NYC',30),(2,'LA',30),(3,'NYC',41),(4,'NYC',30)",
            )
            .unwrap();

        // Non-unique single-column index: duplicates allowed; a seek returns all.
        env.engine
            .execute_autocommit("CREATE INDEX p_city ON p (city)")
            .unwrap();
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT id FROM p WHERE city = 'NYC' ORDER BY id")
                    .unwrap()
            ),
            vec![
                vec![Value::Int64(1)],
                vec![Value::Int64(3)],
                vec![Value::Int64(4)]
            ]
        );

        // A multi-column UNIQUE index over duplicate pairs ((NYC,30) twice) fails.
        assert!(
            env.engine
                .execute_autocommit("CREATE UNIQUE INDEX p_city_age ON p (city, age)")
                .is_err()
        );

        // Multi-column UNIQUE on distinct pairs: enforced, and seekable when both
        // columns are pinned.
        env.engine
            .execute_autocommit("CREATE TABLE q (a BIGINT, b BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO q VALUES (1,1),(1,2),(2,1)")
            .unwrap();
        env.engine
            .execute_autocommit("CREATE UNIQUE INDEX q_ab ON q (a, b)")
            .unwrap();
        assert!(
            env.engine
                .execute_autocommit("INSERT INTO q VALUES (1,2)")
                .is_err()
        );
        assert_eq!(
            affected(
                env.engine
                    .execute_autocommit("INSERT INTO q VALUES (2,2)")
                    .unwrap()
            ),
            1
        );
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT a FROM q WHERE a = 1 AND b = 2")
                    .unwrap()
            ),
            vec![vec![Value::Int64(1)]]
        );
    }

    #[test]
    fn correlated_subqueries() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE emp (id BIGINT, dept BIGINT, salary BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO emp VALUES (1,10,100),(2,10,200),(3,20,50),(4,20,90)")
            .unwrap();

        // Correlated scalar subquery: each employee paid the max in their dept.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit(
                        "SELECT id FROM emp e \
                         WHERE e.salary = (SELECT MAX(salary) FROM emp WHERE dept = e.dept) \
                         ORDER BY id"
                    )
                    .unwrap()
            ),
            vec![vec![Value::Int64(2)], vec![Value::Int64(4)]]
        );

        // Correlated EXISTS: employees in a dept that has someone paid over 150.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit(
                        "SELECT id FROM emp e WHERE EXISTS \
                         (SELECT 1 FROM emp x WHERE x.dept = e.dept AND x.salary > 150) ORDER BY id"
                    )
                    .unwrap()
            ),
            vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
        );

        // Correlated NOT EXISTS is the complement.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit(
                        "SELECT id FROM emp e WHERE NOT EXISTS \
                         (SELECT 1 FROM emp x WHERE x.dept = e.dept AND x.salary > 150) ORDER BY id"
                    )
                    .unwrap()
            ),
            vec![vec![Value::Int64(3)], vec![Value::Int64(4)]]
        );
    }

    #[test]
    fn set_operations() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE a (x BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("CREATE TABLE b (x BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO a VALUES (1),(2),(2),(3)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO b VALUES (2),(3),(3),(4)")
            .unwrap();

        // UNION dedupes across both sides; ORDER BY binds to the combined output.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT x FROM a UNION SELECT x FROM b ORDER BY x")
                    .unwrap()
            ),
            vec![
                vec![Value::Int64(1)],
                vec![Value::Int64(2)],
                vec![Value::Int64(3)],
                vec![Value::Int64(4)],
            ]
        );

        // UNION ALL keeps every copy (4 + 4 rows).
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT x FROM a UNION ALL SELECT x FROM b")
                    .unwrap()
            )
            .len(),
            8
        );

        // INTERSECT: distinct rows on both sides.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT x FROM a INTERSECT SELECT x FROM b ORDER BY x")
                    .unwrap()
            ),
            vec![vec![Value::Int64(2)], vec![Value::Int64(3)]]
        );

        // INTERSECT ALL: min multiplicity per row — one 2 (a:2,b:1) and one 3 (a:1,b:2).
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT x FROM a INTERSECT ALL SELECT x FROM b ORDER BY x")
                    .unwrap()
            ),
            vec![vec![Value::Int64(2)], vec![Value::Int64(3)]]
        );

        // EXCEPT: distinct left rows absent from the right (only 1).
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT x FROM a EXCEPT SELECT x FROM b ORDER BY x")
                    .unwrap()
            ),
            vec![vec![Value::Int64(1)]]
        );

        // EXCEPT ALL: left multiplicities minus right — 1 once, and one extra 2.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT x FROM a EXCEPT ALL SELECT x FROM b ORDER BY x")
                    .unwrap()
            ),
            vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
        );

        // Mismatched arity is rejected.
        assert!(
            env.engine
                .execute_autocommit("SELECT x, x FROM a UNION SELECT x FROM b")
                .is_err()
        );
    }

    #[test]
    fn insert_select() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE src (id BIGINT, name TEXT, qty BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("CREATE TABLE dst (id BIGINT PRIMARY KEY, name TEXT, qty BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO src VALUES (1,'a',10),(2,'b',20),(3,'c',30)")
            .unwrap();

        // INSERT … SELECT with a WHERE filter; the PK index is maintained.
        assert_eq!(
            env.engine
                .execute_autocommit("INSERT INTO dst SELECT id, name, qty FROM src WHERE qty >= 20")
                .unwrap(),
            Outcome::Insert { count: 2 }
        );
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT id, name FROM dst ORDER BY id")
                    .unwrap()
            ),
            vec![
                vec![Value::Int64(2), Value::Text("b".into())],
                vec![Value::Int64(3), Value::Text("c".into())],
            ]
        );

        // A column subset fills the named columns; the rest default to NULL.
        env.engine
            .execute_autocommit("INSERT INTO dst (id, name) SELECT id, name FROM src WHERE id = 1")
            .unwrap();
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit("SELECT qty FROM dst WHERE id = 1")
                    .unwrap()
            ),
            vec![vec![Value::Null]]
        );

        // A duplicate primary key from the source is rejected.
        assert!(
            env.engine
                .execute_autocommit("INSERT INTO dst SELECT id, name, qty FROM src WHERE id = 2")
                .is_err()
        );

        // Arity mismatch between source and target is rejected.
        assert!(
            env.engine
                .execute_autocommit("INSERT INTO dst SELECT id, name FROM src")
                .is_err()
        );
    }

    #[test]
    fn pk_range_seek() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,'e')")
            .unwrap();

        let ids = |sql: &str| -> Vec<i64> {
            rows(env.engine.execute_autocommit(sql).unwrap())
                .into_iter()
                .map(|r| match r[0] {
                    Value::Int64(n) => n,
                    ref other => panic!("expected int id, got {other:?}"),
                })
                .collect()
        };

        // Each comparison form over the primary key.
        assert_eq!(ids("SELECT id FROM t WHERE id > 3 ORDER BY id"), vec![4, 5]);
        assert_eq!(
            ids("SELECT id FROM t WHERE id >= 3 ORDER BY id"),
            vec![3, 4, 5]
        );
        assert_eq!(ids("SELECT id FROM t WHERE id < 3 ORDER BY id"), vec![1, 2]);
        assert_eq!(
            ids("SELECT id FROM t WHERE id <= 2 ORDER BY id"),
            vec![1, 2]
        );
        assert_eq!(
            ids("SELECT id FROM t WHERE id BETWEEN 2 AND 4 ORDER BY id"),
            vec![2, 3, 4]
        );
        // Two bounds intersect to a half-open interval.
        assert_eq!(
            ids("SELECT id FROM t WHERE id > 1 AND id < 4 ORDER BY id"),
            vec![2, 3]
        );
        // A literal on the left flips the operator (`3 < id` == `id > 3`).
        assert_eq!(ids("SELECT id FROM t WHERE 3 < id ORDER BY id"), vec![4, 5]);
        // A residual predicate is still applied to the seeked candidates.
        assert_eq!(
            ids("SELECT id FROM t WHERE id >= 2 AND name = 'd'"),
            vec![4]
        );
        // An empty interval yields nothing.
        assert_eq!(
            ids("SELECT id FROM t WHERE id > 4 AND id < 2"),
            Vec::<i64>::new()
        );
    }

    #[test]
    fn scalar_function_topup() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE t (x BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO t VALUES (1)")
            .unwrap();
        let scalar = |sql: &str| -> Value {
            rows(env.engine.execute_autocommit(sql).unwrap())
                .remove(0)
                .remove(0)
        };
        let dbl = |v: Value| -> f64 {
            match v {
                Value::Double(d) => d,
                Value::Int64(n) => n as f64,
                other => panic!("expected number, got {other:?}"),
            }
        };
        let approx = |sql: &str, want: f64| {
            let got = dbl(scalar(sql));
            assert!((got - want).abs() < 1e-9, "{sql}: got {got}, want {want}");
        };

        // String functions.
        assert_eq!(
            scalar("SELECT LEFT('hello', 3) FROM t"),
            Value::Text("hel".into())
        );
        assert_eq!(
            scalar("SELECT RIGHT('hello', 2) FROM t"),
            Value::Text("lo".into())
        );
        assert_eq!(
            scalar("SELECT REVERSE('abc') FROM t"),
            Value::Text("cba".into())
        );
        assert_eq!(
            scalar("SELECT REPEAT('ab', 3) FROM t"),
            Value::Text("ababab".into())
        );
        assert_eq!(scalar("SELECT SPACE(3) FROM t"), Value::Text("   ".into()));
        assert_eq!(
            scalar("SELECT LPAD('7', 3, '0') FROM t"),
            Value::Text("007".into())
        );
        assert_eq!(
            scalar("SELECT RPAD('7', 3, '*') FROM t"),
            Value::Text("7**".into())
        );
        assert_eq!(
            scalar("SELECT INSTR('hello', 'll') FROM t"),
            Value::Int64(3)
        );
        assert_eq!(
            scalar("SELECT LOCATE('ll', 'hello') FROM t"),
            Value::Int64(3)
        );
        assert_eq!(scalar("SELECT ASCII('A') FROM t"), Value::Int64(65));

        // Numeric functions.
        approx("SELECT SQRT(9) FROM t", 3.0);
        approx("SELECT EXP(0) FROM t", 1.0);
        approx("SELECT LN(1) FROM t", 0.0);
        approx("SELECT LOG10(1000) FROM t", 3.0);
        approx("SELECT LOG2(8) FROM t", 3.0);
        approx("SELECT LOG(2, 8) FROM t", 3.0);
        approx("SELECT TRUNCATE(3.456, 2) FROM t", 3.45);
        approx("SELECT PI() FROM t", std::f64::consts::PI);
        assert_eq!(scalar("SELECT SIGN(-5) FROM t"), Value::Int64(-1));
        assert_eq!(scalar("SELECT SIGN(0) FROM t"), Value::Int64(0));
        assert_eq!(scalar("SELECT GREATEST(3, 7, 2) FROM t"), Value::Int64(7));
        assert_eq!(scalar("SELECT LEAST(3, 7, 2) FROM t"), Value::Int64(2));
        assert_eq!(
            scalar("SELECT GREATEST('a', 'c', 'b') FROM t"),
            Value::Text("c".into())
        );
        assert_eq!(scalar("SELECT GREATEST(1, NULL) FROM t"), Value::Null);

        // Date functions over a known UTC date (2021-07-15 was a Thursday).
        assert_eq!(
            scalar("SELECT QUARTER(CAST('2021-07-15' AS TIMESTAMP)) FROM t"),
            Value::Int64(3)
        );
        assert_eq!(
            scalar("SELECT DAYOFWEEK(CAST('2021-07-15' AS TIMESTAMP)) FROM t"),
            Value::Int64(5)
        );
        assert_eq!(
            scalar("SELECT DAYOFYEAR(CAST('2021-07-15' AS TIMESTAMP)) FROM t"),
            Value::Int64(196)
        );
        assert_eq!(
            scalar("SELECT UNIX_TIMESTAMP(CAST('1970-01-02' AS TIMESTAMP)) FROM t"),
            Value::Int64(86_400)
        );
        // FROM_UNIXTIME round-trips back through the calendar.
        assert_eq!(
            scalar("SELECT YEAR(FROM_UNIXTIME(0)) FROM t"),
            Value::Int64(1970)
        );
    }

    #[test]
    fn common_table_expressions() {
        let env = env();
        env.engine
            .execute_autocommit("CREATE TABLE emp (id BIGINT, dept BIGINT, salary BIGINT)")
            .unwrap();
        env.engine
            .execute_autocommit("INSERT INTO emp VALUES (1,10,100),(2,10,200),(3,20,50),(4,20,90)")
            .unwrap();

        // A single CTE used as the FROM source.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit(
                        "WITH hi AS (SELECT id FROM emp WHERE salary >= 100) \
                         SELECT id FROM hi ORDER BY id"
                    )
                    .unwrap()
            ),
            vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
        );

        // A CTE that references an earlier CTE.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit(
                        "WITH a AS (SELECT id, salary FROM emp WHERE dept = 10), \
                              b AS (SELECT id FROM a WHERE salary > 150) \
                         SELECT id FROM b"
                    )
                    .unwrap()
            ),
            vec![vec![Value::Int64(2)]]
        );

        // An aggregate CTE joined back to a base table.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit(
                        "WITH d AS (SELECT dept, COUNT(*) AS n FROM emp GROUP BY dept) \
                         SELECT e.id, d.n FROM emp e JOIN d ON e.dept = d.dept WHERE e.id = 1"
                    )
                    .unwrap()
            ),
            vec![vec![Value::Int64(1), Value::Int64(2)]]
        );

        // A CTE referenced from a WHERE subquery.
        assert_eq!(
            rows(
                env.engine
                    .execute_autocommit(
                        "WITH big AS (SELECT id FROM emp WHERE salary >= 100) \
                         SELECT id FROM emp WHERE id IN (SELECT id FROM big) ORDER BY id"
                    )
                    .unwrap()
            ),
            vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
        );
    }
}
