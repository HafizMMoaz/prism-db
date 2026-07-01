//! The persistent object catalog: which named objects exist and where.
//!
//! Each table, document collection, and KV namespace is recorded as one entry in
//! a reserved system heap in the unified record store, so the name→heap mapping
//! (and a table's schema) is WAL-logged and recovered like any other data. On
//! open, [`crate::Database`] scans these entries to repopulate its in-memory
//! maps. Entries are encoded with the little-endian protocol codec.
//!
//! Records are append-only and replayed in order on open, so the last record
//! per object wins: an `Upsert` installs it and a `Delete` tombstone (from
//! `DROP TABLE`) removes it. Catalog writes commit in their own transaction
//! (DDL is not yet transactional with surrounding data).

use prism_protocol::codec::{Reader, Writer};
use prism_sql::{Column, ForeignKey, Type, Value};

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
    /// A logical view (carries its `SELECT` text in `view_sql`).
    View,
}

impl ObjectKind {
    fn code(self) -> u8 {
        match self {
            ObjectKind::Table => 1,
            ObjectKind::Collection => 2,
            ObjectKind::Namespace => 3,
            ObjectKind::View => 4,
        }
    }
    fn from_code(v: u8) -> Result<Self> {
        match v {
            1 => Ok(ObjectKind::Table),
            2 => Ok(ObjectKind::Collection),
            3 => Ok(ObjectKind::Namespace),
            4 => Ok(ObjectKind::View),
            other => Err(ServerError::Corrupt(format!("bad object kind {other}"))),
        }
    }
}

/// Whether a catalog record creates/updates an object or removes it (a
/// tombstone). The records are append-only and replayed in order on open, so
/// the last record per object wins — giving `DROP` without rewriting history.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CatalogOp {
    /// Create or update the object.
    Upsert,
    /// Remove the object (`DROP`).
    Delete,
}

impl CatalogOp {
    fn code(self) -> u8 {
        match self {
            CatalogOp::Upsert => 0,
            CatalogOp::Delete => 1,
        }
    }
    fn from_code(v: u8) -> Result<Self> {
        match v {
            0 => Ok(CatalogOp::Upsert),
            1 => Ok(CatalogOp::Delete),
            other => Err(ServerError::Corrupt(format!("bad catalog op {other}"))),
        }
    }
}

/// A persisted secondary index: its name, the indexed column positions, whether
/// it is UNIQUE, and its B+tree root page.
#[derive(Clone, Debug)]
pub struct IndexMeta {
    /// Index name.
    pub name: String,
    /// Indexed column positions in the row, in index order.
    pub columns: Vec<u32>,
    /// Whether the index enforces uniqueness.
    pub unique: bool,
    /// The index B+tree's root page.
    pub root: u64,
}

/// One catalog entry: a named object and its heap, plus a schema for tables and
/// an index-tree root for KV namespaces.
#[derive(Clone, Debug)]
pub struct CatalogEntry {
    /// Create/update vs remove. Defaults to `Upsert` when absent (records
    /// written before `DROP` support).
    pub op: CatalogOp,
    /// The object kind.
    pub kind: ObjectKind,
    /// The object name.
    pub name: String,
    /// The heap holding the object's records.
    pub heap: u64,
    /// The index tree's root page: a KV namespace's hash/ordered index, or a
    /// table's primary-key index. 0 = none.
    pub root_page: u64,
    /// The primary-key column index (tables only).
    pub primary_key: Option<u32>,
    /// Column schema (tables only; empty otherwise).
    pub columns: Vec<Column>,
    /// Secondary (UNIQUE) indexes (tables only; empty otherwise).
    pub indexes: Vec<IndexMeta>,
    /// `CHECK` constraint predicates, as SQL text (tables only; empty otherwise).
    pub checks: Vec<String>,
    /// `FOREIGN KEY` constraints (tables only; empty otherwise).
    pub foreign_keys: Vec<ForeignKey>,
    /// A view's `SELECT` text (views only; empty otherwise).
    pub view_sql: String,
}

impl CatalogEntry {
    /// Encode to the catalog record payload.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut w = Writer::new();
        w.put_u8(self.kind.code());
        w.put_u64(self.heap);
        w.put_u64(self.root_page);
        // u64::MAX encodes "no primary key".
        w.put_u64(self.primary_key.map_or(u64::MAX, u64::from));
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
        // The op is appended last so an `Upsert` record stays byte-identical to
        // the original create-only format.
        w.put_u8(self.op.code());
        // Secondary indexes follow the op (records without them decode as none).
        let nidx: u16 = self
            .indexes
            .len()
            .try_into()
            .map_err(|_| ServerError::Corrupt("too many indexes".into()))?;
        w.put_u16(nidx);
        for ix in &self.indexes {
            w.put_str_u16("catalog.index", &ix.name)?;
            let ncols: u16 = ix
                .columns
                .len()
                .try_into()
                .map_err(|_| ServerError::Corrupt("too many index columns".into()))?;
            w.put_u16(ncols);
            for c in &ix.columns {
                w.put_u64(u64::from(*c));
            }
            w.put_u8(u8::from(ix.unique));
            w.put_u64(ix.root);
        }
        // Column DEFAULT values follow the indexes (column index -> literal).
        // Only columns that have a default are written; a record without this
        // trailing section (older, or no defaults) decodes as all-None.
        let defaults: Vec<(usize, &Value)> = self
            .columns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.default.as_ref().map(|v| (i, v)))
            .collect();
        let ndef: u16 = defaults
            .len()
            .try_into()
            .map_err(|_| ServerError::Corrupt("too many defaults".into()))?;
        w.put_u16(ndef);
        for (i, v) in defaults {
            w.put_u16(i as u16);
            encode_value(&mut w, v)?;
        }
        // CHECK predicates follow the defaults (records without this section
        // decode as none).
        let nchk: u16 = self
            .checks
            .len()
            .try_into()
            .map_err(|_| ServerError::Corrupt("too many checks".into()))?;
        w.put_u16(nchk);
        for c in &self.checks {
            w.put_str_u16("catalog.check", c)?;
        }
        // FOREIGN KEYs follow the checks (records without this section decode as
        // none). Column positions fit u16.
        let nfk: u16 = self
            .foreign_keys
            .len()
            .try_into()
            .map_err(|_| ServerError::Corrupt("too many foreign keys".into()))?;
        w.put_u16(nfk);
        for fk in &self.foreign_keys {
            w.put_u16(fk.columns.len() as u16);
            for &c in &fk.columns {
                w.put_u16(c as u16);
            }
            w.put_str_u16("catalog.fk_table", &fk.ref_table)?;
            w.put_u16(fk.ref_columns.len() as u16);
            for &c in &fk.ref_columns {
                w.put_u16(c as u16);
            }
        }
        // A view's SELECT text follows the foreign keys (empty for non-views, and
        // absent entirely in records written before views existed).
        w.put_str_u16("catalog.view_sql", &self.view_sql)?;
        Ok(w.into_vec())
    }

    /// Decode a catalog record payload.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let kind = ObjectKind::from_code(r.get_u8("catalog.kind")?)?;
        let heap = r.get_u64("catalog.heap")?;
        let root_page = r.get_u64("catalog.root_page")?;
        let pk = r.get_u64("catalog.primary_key")?;
        let primary_key = (pk != u64::MAX).then_some(pk as u32);
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
                default: None,
            });
        }
        // Records from before `DROP` support end here and decode as `Upsert`.
        let op = if r.is_empty() {
            CatalogOp::Upsert
        } else {
            CatalogOp::from_code(r.get_u8("catalog.op")?)?
        };
        // Secondary indexes follow the op; records without them decode as none.
        let indexes = if r.is_empty() {
            Vec::new()
        } else {
            let nidx = r.get_u16("catalog.nindexes")?;
            let mut v = Vec::with_capacity(nidx as usize);
            for _ in 0..nidx {
                let name = r.get_str_u16("catalog.index")?;
                let ncols = r.get_u16("catalog.index_ncols")?;
                let mut columns = Vec::with_capacity(ncols as usize);
                for _ in 0..ncols {
                    columns.push(r.get_u64("catalog.index_col")? as u32);
                }
                let unique = r.get_u8("catalog.index_unique")? != 0;
                let root = r.get_u64("catalog.index_root")?;
                v.push(IndexMeta {
                    name,
                    columns,
                    unique,
                    root,
                });
            }
            v
        };
        // Column DEFAULT values follow the indexes; absent in older records.
        if !r.is_empty() {
            let ndef = r.get_u16("catalog.ndefaults")?;
            for _ in 0..ndef {
                let idx = r.get_u16("catalog.default_col")? as usize;
                let value = decode_value(&mut r)?;
                if let Some(col) = columns.get_mut(idx) {
                    col.default = Some(value);
                }
            }
        }
        // CHECK predicates follow the defaults; absent in older records.
        let checks = if r.is_empty() {
            Vec::new()
        } else {
            let nchk = r.get_u16("catalog.nchecks")?;
            let mut v = Vec::with_capacity(nchk as usize);
            for _ in 0..nchk {
                v.push(r.get_str_u16("catalog.check")?);
            }
            v
        };
        // FOREIGN KEYs follow the checks; absent in older records.
        let foreign_keys = if r.is_empty() {
            Vec::new()
        } else {
            let nfk = r.get_u16("catalog.nfk")?;
            let mut v = Vec::with_capacity(nfk as usize);
            for _ in 0..nfk {
                let nchild = r.get_u16("catalog.fk_nchild")?;
                let mut columns = Vec::with_capacity(nchild as usize);
                for _ in 0..nchild {
                    columns.push(r.get_u16("catalog.fk_child")? as usize);
                }
                let ref_table = r.get_str_u16("catalog.fk_table")?;
                let nref = r.get_u16("catalog.fk_nref")?;
                let mut ref_columns = Vec::with_capacity(nref as usize);
                for _ in 0..nref {
                    ref_columns.push(r.get_u16("catalog.fk_ref")? as usize);
                }
                v.push(ForeignKey {
                    columns,
                    ref_table,
                    ref_columns,
                });
            }
            v
        };
        // A view's SELECT text follows the foreign keys; absent in older records.
        let view_sql = if r.is_empty() {
            String::new()
        } else {
            r.get_str_u16("catalog.view_sql")?
        };
        Ok(Self {
            op,
            kind,
            name,
            heap,
            root_page,
            primary_key,
            columns,
            indexes,
            checks,
            foreign_keys,
            view_sql,
        })
    }
}

/// Encode a literal default [`Value`] (a one-byte tag then its payload).
fn encode_value(w: &mut Writer, v: &Value) -> Result<()> {
    match v {
        Value::Null => w.put_u8(0),
        Value::Bool(b) => {
            w.put_u8(1);
            w.put_u8(u8::from(*b));
        }
        Value::Int64(n) => {
            w.put_u8(2);
            w.put_u64(*n as u64);
        }
        Value::Double(d) => {
            w.put_u8(3);
            w.put_u64(d.to_bits());
        }
        Value::Timestamp(t) => {
            w.put_u8(4);
            w.put_u64(*t as u64);
        }
        Value::Text(s) => {
            w.put_u8(5);
            w.put_str_u16("catalog.default_text", s)?;
        }
    }
    Ok(())
}

/// Decode a literal default [`Value`] written by [`encode_value`].
fn decode_value(r: &mut Reader) -> Result<Value> {
    Ok(match r.get_u8("catalog.default_tag")? {
        0 => Value::Null,
        1 => Value::Bool(r.get_u8("catalog.default_bool")? != 0),
        2 => Value::Int64(r.get_u64("catalog.default_i64")? as i64),
        3 => Value::Double(f64::from_bits(r.get_u64("catalog.default_f64")?)),
        4 => Value::Timestamp(r.get_u64("catalog.default_ts")? as i64),
        5 => Value::Text(r.get_str_u16("catalog.default_text")?),
        other => {
            return Err(ServerError::Corrupt(format!(
                "bad default value tag {other}"
            )));
        }
    })
}

/// Whether a persisted user record creates/updates an account or removes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UserOp {
    /// Create or update the account.
    Upsert,
    /// Remove the account (a tombstone).
    Delete,
}

impl UserOp {
    fn code(self) -> u8 {
        match self {
            UserOp::Upsert => 1,
            UserOp::Delete => 2,
        }
    }
    fn from_code(v: u8) -> Result<Self> {
        match v {
            1 => Ok(UserOp::Upsert),
            2 => Ok(UserOp::Delete),
            other => Err(ServerError::Corrupt(format!("bad user op {other}"))),
        }
    }
}

/// One persisted user account, written append-only to a reserved system heap.
/// On open the records are replayed in order, so the last `Upsert` for a
/// username wins and a `Delete` tombstones it — giving durable accounts and
/// grants without in-place updates.
#[derive(Clone, Debug)]
pub struct UserEntry {
    /// Create/update vs remove.
    pub op: UserOp,
    /// The account name.
    pub username: String,
    /// The account's stable OID.
    pub oid: u64,
    /// The global privilege bitmask (see `auth::Privileges`).
    pub privileges: u8,
    /// The scrypt PHC hash (empty for a `Delete`).
    pub phc: String,
    /// Per-database privilege overrides: `(database, bitmask)`. A full snapshot
    /// is written on every change. Absent in records from before the feature,
    /// which decode as an empty list.
    pub db_grants: Vec<(String, u8)>,
}

impl UserEntry {
    /// Encode to the user-record payload. The per-database grants are appended
    /// last so a record without them is byte-identical to the original format.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut w = Writer::new();
        w.put_u8(self.op.code());
        w.put_u64(self.oid);
        w.put_u8(self.privileges);
        w.put_str_u16("user.name", &self.username)?;
        w.put_str_u16("user.phc", &self.phc)?;
        let count: u16 = self
            .db_grants
            .len()
            .try_into()
            .map_err(|_| ServerError::Corrupt("too many database grants".into()))?;
        w.put_u16(count);
        for (db, bits) in &self.db_grants {
            w.put_str_u16("user.grant.db", db)?;
            w.put_u8(*bits);
        }
        Ok(w.into_vec())
    }

    /// Decode a user-record payload. Records written before per-database grants
    /// end after the PHC; their grant list is empty.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let op = UserOp::from_code(r.get_u8("user.op")?)?;
        let oid = r.get_u64("user.oid")?;
        let privileges = r.get_u8("user.privileges")?;
        let username = r.get_str_u16("user.name")?;
        let phc = r.get_str_u16("user.phc")?;
        let db_grants = if r.is_empty() {
            Vec::new()
        } else {
            let count = r.get_u16("user.grant.count")?;
            let mut grants = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let db = r.get_str_u16("user.grant.db")?;
                let bits = r.get_u8("user.grant.bits")?;
                grants.push((db, bits));
            }
            grants
        };
        Ok(Self {
            op,
            username,
            oid,
            privileges,
            phc,
            db_grants,
        })
    }
}

fn type_code(ty: Type) -> u8 {
    match ty {
        Type::Bool => 0,
        Type::Int64 => 1,
        Type::Text => 2,
        Type::Double => 3,
        Type::Timestamp => 4,
    }
}

fn type_from_code(code: u8) -> Result<Type> {
    match code {
        0 => Ok(Type::Bool),
        1 => Ok(Type::Int64),
        2 => Ok(Type::Text),
        3 => Ok(Type::Double),
        4 => Ok(Type::Timestamp),
        other => Err(ServerError::Corrupt(format!("bad column type {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_round_trips() {
        let table = CatalogEntry {
            op: CatalogOp::Upsert,
            kind: ObjectKind::Table,
            name: "accounts".into(),
            heap: 1000,
            root_page: 42,
            primary_key: Some(0),
            columns: vec![
                Column {
                    name: "id".into(),
                    ty: Type::Int64,
                    nullable: false,
                    default: None,
                },
                Column {
                    name: "owner".into(),
                    ty: Type::Text,
                    nullable: true,
                    default: Some(Value::Text("anon".into())),
                },
            ],
            indexes: vec![IndexMeta {
                name: "accounts_owner_idx".into(),
                columns: vec![1],
                unique: true,
                root: 99,
            }],
            checks: vec!["id > 0".into()],
            foreign_keys: vec![ForeignKey {
                columns: vec![1],
                ref_table: "owners".into(),
                ref_columns: vec![0],
            }],
            view_sql: String::new(),
        };
        let decoded = CatalogEntry::decode(&table.encode().unwrap()).unwrap();
        assert_eq!(decoded.op, CatalogOp::Upsert);
        assert_eq!(decoded.kind, ObjectKind::Table);
        assert_eq!(decoded.name, "accounts");
        assert_eq!(decoded.heap, 1000);
        assert_eq!(decoded.root_page, 42);
        assert_eq!(decoded.primary_key, Some(0));
        assert_eq!(decoded.columns.len(), 2);
        assert_eq!(decoded.columns[1].name, "owner");
        assert_eq!(decoded.columns[1].ty, Type::Text);
        assert_eq!(decoded.indexes.len(), 1);
        assert_eq!(decoded.indexes[0].name, "accounts_owner_idx");
        assert_eq!(decoded.indexes[0].columns, vec![1]);
        assert!(decoded.indexes[0].unique);
        assert_eq!(decoded.indexes[0].root, 99);
        assert_eq!(decoded.columns[0].default, None);
        assert_eq!(decoded.columns[1].default, Some(Value::Text("anon".into())));
        assert_eq!(decoded.checks, vec!["id > 0".to_string()]);
        assert_eq!(decoded.foreign_keys.len(), 1);
        assert_eq!(decoded.foreign_keys[0].columns, vec![1]);
        assert_eq!(decoded.foreign_keys[0].ref_table, "owners");
        assert_eq!(decoded.foreign_keys[0].ref_columns, vec![0]);

        let ns = CatalogEntry {
            op: CatalogOp::Delete,
            kind: ObjectKind::Namespace,
            name: "sessions".into(),
            heap: 1 << 41,
            root_page: 77,
            primary_key: None,
            columns: vec![],
            indexes: vec![],
            checks: vec![],
            foreign_keys: vec![],
            view_sql: String::new(),
        };
        let decoded = CatalogEntry::decode(&ns.encode().unwrap()).unwrap();
        assert_eq!(decoded.op, CatalogOp::Delete);
        assert_eq!(decoded.kind, ObjectKind::Namespace);
        assert_eq!(decoded.heap, 1 << 41);
        assert_eq!(decoded.root_page, 77);
        assert_eq!(decoded.primary_key, None);
        assert!(decoded.columns.is_empty());

        let view = CatalogEntry {
            op: CatalogOp::Upsert,
            kind: ObjectKind::View,
            name: "active_accounts".into(),
            heap: 0,
            root_page: 0,
            primary_key: None,
            columns: vec![],
            indexes: vec![],
            checks: vec![],
            foreign_keys: vec![],
            view_sql: "SELECT * FROM accounts WHERE id > 0".into(),
        };
        let decoded = CatalogEntry::decode(&view.encode().unwrap()).unwrap();
        assert_eq!(decoded.op, CatalogOp::Upsert);
        assert_eq!(decoded.kind, ObjectKind::View);
        assert_eq!(decoded.name, "active_accounts");
        assert_eq!(decoded.view_sql, "SELECT * FROM accounts WHERE id > 0");
    }

    #[test]
    fn legacy_record_without_op_decodes_as_upsert() {
        // An old create-only record has no trailing op byte.
        let mut bytes = CatalogEntry {
            op: CatalogOp::Upsert,
            kind: ObjectKind::Table,
            name: "t".into(),
            heap: 1000,
            root_page: 0,
            primary_key: None,
            columns: vec![],
            indexes: vec![],
            checks: vec![],
            foreign_keys: vec![],
            view_sql: String::new(),
        }
        .encode()
        .unwrap();
        // The original create-only format ended after the columns; drop the
        // trailing op byte and the (empty) index / default / check / foreign-key
        // count u16s and the (empty) view-text length u16 that now follow it.
        bytes.truncate(bytes.len() - 11);
        let decoded = CatalogEntry::decode(&bytes).unwrap();
        assert_eq!(decoded.op, CatalogOp::Upsert);
        assert_eq!(decoded.name, "t");
    }

    #[test]
    fn rejects_corrupt_bytes() {
        assert!(CatalogEntry::decode(&[]).is_err());
        assert!(CatalogEntry::decode(&[9, 0, 0, 0, 0, 0, 0, 0, 0]).is_err()); // bad kind
    }
}
