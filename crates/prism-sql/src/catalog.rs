//! The in-memory relational catalog: tables, their columns, and their heap.
//!
//! Scope (this slice): held in memory only - schema persistence across restart
//! arrives with the full system-table catalog. Table heaps are allocated from a
//! simple counter, disjoint from KV/document heaps by convention.

use std::collections::HashMap;
use std::sync::Mutex;

use prism_core::store::HeapId;
use prism_storage::PageId;

use crate::error::{Result, SqlError};
use crate::types::{Type, Value};

/// A column definition.
#[derive(Clone, Debug)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// Column type.
    pub ty: Type,
    /// Whether NULLs are allowed.
    pub nullable: bool,
    /// The literal value to use when an `INSERT` omits this column (`DEFAULT`).
    /// `None` means the implicit default of NULL.
    pub default: Option<Value>,
}

/// A secondary index: a named, durable B+tree over one or more columns. A
/// `UNIQUE` index keys directly on the (composite) column value and enforces
/// uniqueness; a non-unique index appends the row id to the key (so duplicates
/// coexist) and is range-scanned for lookups. Both accelerate equality seeks.
#[derive(Clone, Debug)]
pub struct IndexDef {
    /// Index name (unique within a database).
    pub name: String,
    /// The indexed columns' positions in the row, in index order.
    pub columns: Vec<usize>,
    /// Whether the index enforces uniqueness on the composite key.
    pub unique: bool,
    /// The index B+tree's root page.
    pub root: PageId,
}

/// A `FOREIGN KEY` constraint: this table's `columns` must match an existing row
/// in `ref_table` on its `ref_columns` (positions resolved at definition time).
#[derive(Clone, Debug)]
pub struct ForeignKey {
    /// The referencing (child) column positions in this table's row.
    pub columns: Vec<usize>,
    /// The referenced (parent) table name.
    pub ref_table: String,
    /// The referenced column positions in the parent table's row.
    pub ref_columns: Vec<usize>,
}

/// A table's schema and physical heap.
#[derive(Clone, Debug)]
pub struct Table {
    /// Table name.
    pub name: String,
    /// The heap holding this table's rows.
    pub heap: HeapId,
    /// Columns, in declared order.
    pub columns: Vec<Column>,
    /// The `PRIMARY KEY` column index, if the table has one.
    pub primary_key: Option<usize>,
    /// The root page of the primary-key B+tree index, if any.
    pub index_root: Option<PageId>,
    /// Secondary (`UNIQUE`) indexes on this table.
    pub indexes: Vec<IndexDef>,
    /// `CHECK` constraint predicates (SQL text), evaluated on `INSERT`/`UPDATE`.
    pub checks: Vec<String>,
    /// `FOREIGN KEY` constraints referencing other tables.
    pub foreign_keys: Vec<ForeignKey>,
}

impl Table {
    /// The position of `column` in the row, if present.
    pub fn column_index(&self, column: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == column)
    }

    /// The column types, in order.
    pub fn types(&self) -> Vec<Type> {
        self.columns.iter().map(|c| c.ty).collect()
    }
}

/// The in-memory catalog of relational tables.
pub struct Catalog {
    tables: Mutex<HashMap<String, Table>>,
    /// Logical views: name → the view's `SELECT` text. A view is expanded into a
    /// derived subquery at query time (it owns no heap of its own).
    views: Mutex<HashMap<String, String>>,
    next_heap: Mutex<u64>,
}

impl Catalog {
    /// Create an empty catalog. Table heaps start at `first_heap`.
    pub fn new(first_heap: u64) -> Self {
        Self {
            tables: Mutex::new(HashMap::new()),
            views: Mutex::new(HashMap::new()),
            next_heap: Mutex::new(first_heap),
        }
    }

    /// Register a new table, allocating its heap. Errors if the name is taken.
    /// `primary_key`/`index_root` describe the optional primary-key index.
    pub fn create_table(
        &self,
        name: &str,
        columns: Vec<Column>,
        primary_key: Option<usize>,
        index_root: Option<PageId>,
        checks: Vec<String>,
        foreign_keys: Vec<ForeignKey>,
    ) -> Result<Table> {
        let mut tables = self.tables.lock().expect("catalog poisoned");
        if tables.contains_key(name) {
            return Err(SqlError::TableExists(name.to_string()));
        }
        let heap = {
            let mut n = self.next_heap.lock().expect("catalog poisoned");
            let h = HeapId(*n);
            *n += 1;
            h
        };
        let table = Table {
            name: name.to_string(),
            heap,
            columns,
            primary_key,
            index_root,
            indexes: Vec::new(),
            checks,
            foreign_keys,
        };
        tables.insert(name.to_string(), table.clone());
        Ok(table)
    }

    /// Remove a table from the catalog. Errors if it does not exist. The heap
    /// allocator is not rewound, so a dropped heap id is never reused.
    pub fn drop_table(&self, name: &str) -> Result<()> {
        let mut tables = self.tables.lock().expect("catalog poisoned");
        if tables.remove(name).is_none() {
            return Err(SqlError::NoSuchTable(name.to_string()));
        }
        Ok(())
    }

    /// Replace a table's columns and primary-key position in place, keeping its
    /// heap and index root (used by `ALTER TABLE` add/drop/rename column).
    pub fn redefine_table(
        &self,
        name: &str,
        columns: Vec<Column>,
        primary_key: Option<usize>,
    ) -> Result<()> {
        let mut tables = self.tables.lock().expect("catalog poisoned");
        let table = tables
            .get_mut(name)
            .ok_or_else(|| SqlError::NoSuchTable(name.to_string()))?;
        table.columns = columns;
        table.primary_key = primary_key;
        Ok(())
    }

    /// Rename a table, keeping its heap, schema, and index (`ALTER TABLE … RENAME
    /// TO`). Errors if the new name is taken or the old name is unknown.
    pub fn rename_table(&self, old: &str, new: &str) -> Result<()> {
        let mut tables = self.tables.lock().expect("catalog poisoned");
        if tables.contains_key(new) {
            return Err(SqlError::TableExists(new.to_string()));
        }
        let mut table = tables
            .remove(old)
            .ok_or_else(|| SqlError::NoSuchTable(old.to_string()))?;
        table.name = new.to_string();
        tables.insert(new.to_string(), table);
        Ok(())
    }

    /// Look up a table by name.
    pub fn table(&self, name: &str) -> Result<Table> {
        self.tables
            .lock()
            .expect("catalog poisoned")
            .get(name)
            .cloned()
            .ok_or_else(|| SqlError::NoSuchTable(name.to_string()))
    }

    /// All table names, sorted (for deterministic enumeration, e.g. dumps).
    pub fn table_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tables
            .lock()
            .expect("catalog poisoned")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Install a table at a known heap (used when reloading a persisted catalog
    /// after restart). Bumps the heap allocator past `heap` so new tables don't
    /// collide. Errors if the name is already registered.
    #[allow(clippy::too_many_arguments)]
    pub fn register_table(
        &self,
        name: &str,
        columns: Vec<Column>,
        heap: HeapId,
        primary_key: Option<usize>,
        index_root: Option<PageId>,
        indexes: Vec<IndexDef>,
        checks: Vec<String>,
        foreign_keys: Vec<ForeignKey>,
    ) -> Result<()> {
        let mut tables = self.tables.lock().expect("catalog poisoned");
        if tables.contains_key(name) {
            return Err(SqlError::TableExists(name.to_string()));
        }
        tables.insert(
            name.to_string(),
            Table {
                name: name.to_string(),
                heap,
                columns,
                primary_key,
                index_root,
                indexes,
                checks,
                foreign_keys,
            },
        );
        let mut n = self.next_heap.lock().expect("catalog poisoned");
        *n = (*n).max(heap.0 + 1);
        Ok(())
    }

    /// Register a secondary index on an existing table. Errors if the table is
    /// unknown or the index name is already taken (in any table).
    pub fn add_index(&self, table: &str, def: IndexDef) -> Result<()> {
        let mut tables = self.tables.lock().expect("catalog poisoned");
        if tables
            .values()
            .any(|t| t.indexes.iter().any(|i| i.name == def.name))
        {
            return Err(SqlError::TableExists(def.name));
        }
        let t = tables
            .get_mut(table)
            .ok_or_else(|| SqlError::NoSuchTable(table.to_string()))?;
        t.indexes.push(def);
        Ok(())
    }

    /// Remove a secondary index by name, returning its table's name. Errors if
    /// no such index exists.
    pub fn drop_index(&self, name: &str) -> Result<String> {
        let mut tables = self.tables.lock().expect("catalog poisoned");
        for t in tables.values_mut() {
            if let Some(pos) = t.indexes.iter().position(|i| i.name == name) {
                t.indexes.remove(pos);
                return Ok(t.name.clone());
            }
        }
        Err(SqlError::NoSuchTable(format!("index {name}")))
    }

    /// A snapshot of all tables (for persisting the catalog).
    pub fn tables_snapshot(&self) -> Vec<Table> {
        self.tables
            .lock()
            .expect("catalog poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Define a view from its `SELECT` text. Errors if a table already uses the
    /// name, or if a view of that name exists and `or_replace` is false.
    pub fn create_view(&self, name: &str, query_sql: String, or_replace: bool) -> Result<()> {
        if self
            .tables
            .lock()
            .expect("catalog poisoned")
            .contains_key(name)
        {
            return Err(SqlError::TableExists(name.to_string()));
        }
        let mut views = self.views.lock().expect("catalog poisoned");
        if views.contains_key(name) && !or_replace {
            return Err(SqlError::TableExists(format!("view {name}")));
        }
        views.insert(name.to_string(), query_sql);
        Ok(())
    }

    /// Install a view at load time (no name-collision or replace checks - the
    /// persisted catalog is trusted). Used when reloading after restart.
    pub fn register_view(&self, name: &str, query_sql: String) {
        self.views
            .lock()
            .expect("catalog poisoned")
            .insert(name.to_string(), query_sql);
    }

    /// Remove a view. Errors if no such view exists.
    pub fn drop_view(&self, name: &str) -> Result<()> {
        if self
            .views
            .lock()
            .expect("catalog poisoned")
            .remove(name)
            .is_none()
        {
            return Err(SqlError::NoSuchTable(format!("view {name}")));
        }
        Ok(())
    }

    /// The `SELECT` text of a view, if `name` names one.
    pub fn view(&self, name: &str) -> Option<String> {
        self.views
            .lock()
            .expect("catalog poisoned")
            .get(name)
            .cloned()
    }

    /// All view names, sorted (for deterministic enumeration).
    pub fn view_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .views
            .lock()
            .expect("catalog poisoned")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }
}
