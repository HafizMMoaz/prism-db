//! The persistent object catalog: which named objects exist and where.
//!
//! Each table, document collection, and KV namespace is recorded as one entry in
//! a reserved system heap in the unified record store, so the name→heap mapping
//! (and a table's schema) is WAL-logged and recovered like any other data. On
//! open, [`crate::Database`] scans these entries to repopulate its in-memory
//! maps. Entries are encoded with the little-endian protocol codec.
//!
//! **Scope (this increment):** create-only — there is no `DROP`, so each object
//! has exactly one entry, and catalog writes commit in their own transaction
//! (DDL is not yet transactional with surrounding data).

use prism_protocol::codec::{Reader, Writer};
use prism_sql::{Column, Type};

use crate::error::{Result, ServerError};

/// The kind of catalog object.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectKind {
    /// A relational table (carries a schema).
    Table,
    /// A document collection.
    Collection,
    /// A KV namespace.
    Namespace,
}

impl ObjectKind {
    fn code(self) -> u8 {
        match self {
            ObjectKind::Table => 1,
            ObjectKind::Collection => 2,
            ObjectKind::Namespace => 3,
        }
    }
    fn from_code(v: u8) -> Result<Self> {
        match v {
            1 => Ok(ObjectKind::Table),
            2 => Ok(ObjectKind::Collection),
            3 => Ok(ObjectKind::Namespace),
            other => Err(ServerError::Corrupt(format!("bad object kind {other}"))),
        }
    }
}

/// One catalog entry: a named object and its heap, plus a schema for tables and
/// an index-tree root for KV namespaces.
#[derive(Clone, Debug)]
pub struct CatalogEntry {
    /// The object kind.
    pub kind: ObjectKind,
    /// The object name.
    pub name: String,
    /// The heap holding the object's records.
    pub heap: u64,
    /// The KV index tree's root page (namespaces only; 0 otherwise).
    pub root_page: u64,
    /// Column schema (tables only; empty otherwise).
    pub columns: Vec<Column>,
}

impl CatalogEntry {
    /// Encode to the catalog record payload.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut w = Writer::new();
        w.put_u8(self.kind.code());
        w.put_u64(self.heap);
        w.put_u64(self.root_page);
        w.put_str_u16("catalog.name", &self.name)?;
        let ncols: u16 = self
            .columns
            .len()
            .try_into()
            .map_err(|_| ServerError::Corrupt("too many columns".into()))?;
        w.put_u16(ncols);
        for col in &self.columns {
            w.put_str_u16("catalog.column", &col.name)?;
            w.put_u8(type_code(col.ty));
            w.put_u8(u8::from(col.nullable));
        }
        Ok(w.into_vec())
    }

    /// Decode a catalog record payload.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let kind = ObjectKind::from_code(r.get_u8("catalog.kind")?)?;
        let heap = r.get_u64("catalog.heap")?;
        let root_page = r.get_u64("catalog.root_page")?;
        let name = r.get_str_u16("catalog.name")?;
        let ncols = r.get_u16("catalog.ncols")?;
        let mut columns = Vec::with_capacity(ncols as usize);
        for _ in 0..ncols {
            let col_name = r.get_str_u16("catalog.column")?;
            let ty = type_from_code(r.get_u8("catalog.column_type")?)?;
            let nullable = r.get_u8("catalog.column_nullable")? != 0;
            columns.push(Column {
                name: col_name,
                ty,
                nullable,
            });
        }
        Ok(Self {
            kind,
            name,
            heap,
            root_page,
            columns,
        })
    }
}

fn type_code(ty: Type) -> u8 {
    match ty {
        Type::Bool => 0,
        Type::Int64 => 1,
        Type::Text => 2,
    }
}

fn type_from_code(code: u8) -> Result<Type> {
    match code {
        0 => Ok(Type::Bool),
        1 => Ok(Type::Int64),
        2 => Ok(Type::Text),
        other => Err(ServerError::Corrupt(format!("bad column type {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_round_trips() {
        let table = CatalogEntry {
            kind: ObjectKind::Table,
            name: "accounts".into(),
            heap: 1000,
            root_page: 0,
            columns: vec![
                Column {
                    name: "id".into(),
                    ty: Type::Int64,
                    nullable: false,
                },
                Column {
                    name: "owner".into(),
                    ty: Type::Text,
                    nullable: true,
                },
            ],
        };
        let decoded = CatalogEntry::decode(&table.encode().unwrap()).unwrap();
        assert_eq!(decoded.kind, ObjectKind::Table);
        assert_eq!(decoded.name, "accounts");
        assert_eq!(decoded.heap, 1000);
        assert_eq!(decoded.columns.len(), 2);
        assert_eq!(decoded.columns[1].name, "owner");
        assert_eq!(decoded.columns[1].ty, Type::Text);

        let ns = CatalogEntry {
            kind: ObjectKind::Namespace,
            name: "sessions".into(),
            heap: 1 << 41,
            root_page: 77,
            columns: vec![],
        };
        let decoded = CatalogEntry::decode(&ns.encode().unwrap()).unwrap();
        assert_eq!(decoded.kind, ObjectKind::Namespace);
        assert_eq!(decoded.heap, 1 << 41);
        assert_eq!(decoded.root_page, 77);
        assert!(decoded.columns.is_empty());
    }

    #[test]
    fn rejects_corrupt_bytes() {
        assert!(CatalogEntry::decode(&[]).is_err());
        assert!(CatalogEntry::decode(&[9, 0, 0, 0, 0, 0, 0, 0, 0]).is_err()); // bad kind
    }
}
