//! The in-memory relational catalog: tables, their columns, and their heap.
//!
//! Scope (this slice): held in memory only — schema persistence across restart
//! arrives with the full system-table catalog. Table heaps are allocated from a
//! simple counter, disjoint from KV/document heaps by convention.

use std::collections::HashMap;
use std::sync::Mutex;

use prism_core::store::HeapId;
use prism_storage::PageId;

use crate::error::{Result, SqlError};
use crate::types::Type;

/// A column definition.
#[derive(Clone, Debug)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// Column type.
    pub ty: Type,
    /// Whether NULLs are allowed.
    pub nullable: bool,
}

/// A secondary index: a named, durable B+tree mapping one column's value to the
/// row's id. Only `UNIQUE` indexes are supported (the tree is a unique key map),
/// so the index both enforces uniqueness and accelerates equality lookups.
#[derive(Clone, Debug)]
pub struct IndexDef {
    /// Index name (unique within a database).
    pub name: String,
    /// The indexed column's position in the row.
    pub column: usize,
    /// Always true for now (non-unique indexes are not yet supported).
    pub unique: bool,
    /// The index B+tree's root page.
    pub root: PageId,
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
    next_heap: Mutex<u64>,
}

impl Catalog {
    /// Create an empty catalog. Table heaps start at `first_heap`.
    pub fn new(first_heap: u64) -> Self {
        Self {
            tables: Mutex::new(HashMap::new()),
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
}
