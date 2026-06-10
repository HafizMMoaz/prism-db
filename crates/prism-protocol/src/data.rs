//! Data-plane payload types shared by the query messages.
//!
//! [`Value`] is the wire form of a scalar (the spec's "TaggedValue"), encoded
//! with the same type tags as document fields (`docs/specs/record-format.md`).
//! [`ColumnDesc`] and the row codec describe a SQL result set. [`DocCommand`]
//! and [`KvCommand`]/[`KvResultBody`] are the op-specific bodies of the document
//! and KV messages — documents, keys, and values are opaque byte strings here,
//! decoded by the engines, not by the protocol.

use crate::codec::{Reader, Writer};
use crate::error::{ProtocolError, Result};

// Value type tags (record-format.md). Array/Object/Decimal are not encoded as
// standalone wire values in v1 — arrays/objects ride inside opaque document
// bytes; Decimal is reserved.
const T_NULL: u8 = 0x00;
const T_BOOL: u8 = 0x01;
const T_INT32: u8 = 0x02;
const T_INT64: u8 = 0x03;
const T_DOUBLE: u8 = 0x04;
const T_STRING: u8 = 0x05;
const T_BINARY: u8 = 0x06;
const T_TIMESTAMP: u8 = 0x09;
const T_OBJECTID: u8 = 0x0A;

/// A scalar wire value (the spec's `TaggedValue`). Numeric encodings are
/// little-endian; see `docs/specs/record-format.md` for the byte layouts.
#[derive(Clone, PartialEq, Debug)]
pub enum Value {
    /// Null.
    Null,
    /// Boolean.
    Bool(bool),
    /// 32-bit signed integer.
    Int32(i32),
    /// 64-bit signed integer.
    Int64(i64),
    /// IEEE-754 double.
    Double(f64),
    /// UTF-8 string.
    Str(String),
    /// Binary blob with a 1-byte subtype.
    Binary {
        /// Application-defined subtype byte.
        subtype: u8,
        /// Raw bytes.
        bytes: Vec<u8>,
    },
    /// Microseconds since the Unix epoch.
    Timestamp(i64),
    /// A 12-byte document id.
    ObjectId([u8; 12]),
}

impl Value {
    /// The record-format type tag for this value.
    pub fn type_tag(&self) -> u8 {
        match self {
            Value::Null => T_NULL,
            Value::Bool(_) => T_BOOL,
            Value::Int32(_) => T_INT32,
            Value::Int64(_) => T_INT64,
            Value::Double(_) => T_DOUBLE,
            Value::Str(_) => T_STRING,
            Value::Binary { .. } => T_BINARY,
            Value::Timestamp(_) => T_TIMESTAMP,
            Value::ObjectId(_) => T_OBJECTID,
        }
    }

    /// Encode just the value bytes (no tag) — used for SQL result cells, where
    /// the type comes from the column descriptor.
    pub fn encode_value(&self, w: &mut Writer) -> Result<()> {
        match self {
            Value::Null => {}
            Value::Bool(b) => w.put_u8(u8::from(*b)),
            Value::Int32(n) => w.put_raw(&n.to_le_bytes()),
            Value::Int64(n) => w.put_raw(&n.to_le_bytes()),
            Value::Double(d) => w.put_raw(&d.to_le_bytes()),
            Value::Str(s) => w.put_str_u32("value.str", s)?,
            Value::Binary { subtype, bytes } => {
                let len: u32 =
                    bytes
                        .len()
                        .try_into()
                        .map_err(|_| ProtocolError::ValueTooLarge {
                            field: "value.binary",
                        })?;
                w.put_u32(len);
                w.put_u8(*subtype);
                w.put_raw(bytes);
            }
            Value::Timestamp(t) => w.put_raw(&t.to_le_bytes()),
            Value::ObjectId(id) => w.put_raw(id),
        }
        Ok(())
    }

    /// Decode value bytes for a known type `tag` (no tag in the stream).
    pub fn decode_value(tag: u8, r: &mut Reader) -> Result<Value> {
        Ok(match tag {
            T_NULL => Value::Null,
            T_BOOL => Value::Bool(r.get_u8("value.bool")? != 0),
            T_INT32 => Value::Int32(i32::from_le_bytes(r.get_array::<4>("value.int32")?)),
            T_INT64 => Value::Int64(i64::from_le_bytes(r.get_array::<8>("value.int64")?)),
            T_DOUBLE => Value::Double(f64::from_le_bytes(r.get_array::<8>("value.double")?)),
            T_STRING => Value::Str(r.get_str_u32("value.str")?),
            T_BINARY => {
                let len = r.get_u32("value.binary_len")? as usize;
                let subtype = r.get_u8("value.binary_subtype")?;
                Value::Binary {
                    subtype,
                    bytes: r.get_raw(len, "value.binary")?.to_vec(),
                }
            }
            T_TIMESTAMP => {
                Value::Timestamp(i64::from_le_bytes(r.get_array::<8>("value.timestamp")?))
            }
            T_OBJECTID => Value::ObjectId(r.get_array::<12>("value.objectid")?),
            other => return Err(ProtocolError::UnknownValueType(other)),
        })
    }

    /// Encode a tagged value (`type_tag` byte then the value) — the wire
    /// `TaggedValue` used for SQL parameters.
    pub fn encode_tagged(&self, w: &mut Writer) -> Result<()> {
        w.put_u8(self.type_tag());
        self.encode_value(w)
    }

    /// Decode a tagged value.
    pub fn decode_tagged(r: &mut Reader) -> Result<Value> {
        let tag = r.get_u8("value.type_tag")?;
        Value::decode_value(tag, r)
    }
}

/// A SQL result column descriptor.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ColumnDesc {
    /// The column name.
    pub name: String,
    /// The record-format type tag of the column.
    pub type_tag: u8,
    /// Whether the column is nullable.
    pub nullable: bool,
}

impl ColumnDesc {
    pub(crate) fn encode(&self, w: &mut Writer) -> Result<()> {
        w.put_str_u16("column.name", &self.name)?;
        w.put_u8(self.type_tag);
        w.put_u8(u8::from(self.nullable));
        Ok(())
    }
    pub(crate) fn decode(r: &mut Reader) -> Result<Self> {
        Ok(Self {
            name: r.get_str_u16("column.name")?,
            type_tag: r.get_u8("column.type_tag")?,
            nullable: r.get_u8("column.nullable")? != 0,
        })
    }
}

/// One result row: a cell per column, `None` for SQL NULL. Each `Some` value's
/// variant must match the corresponding [`ColumnDesc::type_tag`].
pub type Row = Vec<Option<Value>>;

/// Encode `rows` against `columns`: per row, a null bitmap then the non-null
/// cells' value bytes (`docs/specs/wire-protocol.md`, SQL row encoding).
pub(crate) fn encode_rows(columns: &[ColumnDesc], rows: &[Row], w: &mut Writer) -> Result<()> {
    let nb = columns.len().div_ceil(8);
    for row in rows {
        let mut bitmap = vec![0u8; nb];
        for (i, cell) in row.iter().enumerate() {
            if cell.is_none() {
                bitmap[i / 8] |= 1 << (i % 8);
            }
        }
        w.put_raw(&bitmap);
        for v in row.iter().flatten() {
            v.encode_value(w)?;
        }
    }
    Ok(())
}

/// Decode `row_count` rows against `columns`.
pub(crate) fn decode_rows(
    columns: &[ColumnDesc],
    row_count: usize,
    r: &mut Reader,
) -> Result<Vec<Row>> {
    let nb = columns.len().div_ceil(8);
    let mut rows = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        let bitmap = r.get_raw(nb, "row.null_bitmap")?.to_vec();
        let mut row = Row::with_capacity(columns.len());
        for (i, col) in columns.iter().enumerate() {
            let is_null = bitmap[i / 8] & (1 << (i % 8)) != 0;
            row.push(if is_null {
                None
            } else {
                Some(Value::decode_value(col.type_tag, r)?)
            });
        }
        rows.push(row);
    }
    Ok(rows)
}

/// Read a `u16`-length-prefixed opaque byte string (document, key, value).
fn get_blob_u16(r: &mut Reader, needed: &'static str) -> Result<Vec<u8>> {
    Ok(r.get_bytes_u16(needed)?.to_vec())
}
/// Read a `u32`-length-prefixed opaque byte string.
fn get_blob_u32(r: &mut Reader, needed: &'static str) -> Result<Vec<u8>> {
    Ok(r.get_bytes_u32(needed)?.to_vec())
}

/// The op-specific body of a [`crate::Message::DocOp`]. Documents (and query /
/// update / options sub-documents) are opaque tagged-binary blobs to the
/// protocol; the document engine decodes them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DocCommand {
    /// Insert one document.
    InsertOne(Vec<u8>),
    /// Insert many documents.
    InsertMany(Vec<Vec<u8>>),
    /// Find all matching documents.
    Find {
        /// The query document.
        query: Vec<u8>,
        /// The options document.
        options: Vec<u8>,
    },
    /// Find the first matching document.
    FindOne {
        /// The query document.
        query: Vec<u8>,
        /// The options document.
        options: Vec<u8>,
    },
    /// Update the first matching document.
    UpdateOne {
        /// The query document.
        query: Vec<u8>,
        /// The update document.
        update: Vec<u8>,
        /// The options document.
        options: Vec<u8>,
    },
    /// Update all matching documents.
    UpdateMany {
        /// The query document.
        query: Vec<u8>,
        /// The update document.
        update: Vec<u8>,
        /// The options document.
        options: Vec<u8>,
    },
    /// Delete the first matching document.
    DeleteOne {
        /// The query document.
        query: Vec<u8>,
        /// The options document.
        options: Vec<u8>,
    },
    /// Delete all matching documents.
    DeleteMany {
        /// The query document.
        query: Vec<u8>,
        /// The options document.
        options: Vec<u8>,
    },
}

impl DocCommand {
    pub(crate) fn op_type(&self) -> u8 {
        match self {
            DocCommand::InsertOne(_) => 1,
            DocCommand::InsertMany(_) => 2,
            DocCommand::Find { .. } => 3,
            DocCommand::FindOne { .. } => 4,
            DocCommand::UpdateOne { .. } => 5,
            DocCommand::UpdateMany { .. } => 6,
            DocCommand::DeleteOne { .. } => 7,
            DocCommand::DeleteMany { .. } => 8,
        }
    }

    pub(crate) fn encode_body(&self, w: &mut Writer) -> Result<()> {
        match self {
            DocCommand::InsertOne(doc) => w.put_bytes_u32("doc.document", doc)?,
            DocCommand::InsertMany(docs) => {
                let count: u32 = docs
                    .len()
                    .try_into()
                    .map_err(|_| ProtocolError::ValueTooLarge { field: "doc.count" })?;
                w.put_u32(count);
                for d in docs {
                    w.put_bytes_u32("doc.document", d)?;
                }
            }
            DocCommand::Find { query, options } | DocCommand::FindOne { query, options } => {
                w.put_bytes_u32("doc.query", query)?;
                w.put_bytes_u32("doc.options", options)?;
            }
            DocCommand::UpdateOne {
                query,
                update,
                options,
            }
            | DocCommand::UpdateMany {
                query,
                update,
                options,
            } => {
                w.put_bytes_u32("doc.query", query)?;
                w.put_bytes_u32("doc.update", update)?;
                w.put_bytes_u32("doc.options", options)?;
            }
            DocCommand::DeleteOne { query, options }
            | DocCommand::DeleteMany { query, options } => {
                w.put_bytes_u32("doc.query", query)?;
                w.put_bytes_u32("doc.options", options)?;
            }
        }
        Ok(())
    }

    pub(crate) fn decode_body(op_type: u8, r: &mut Reader) -> Result<Self> {
        Ok(match op_type {
            1 => DocCommand::InsertOne(get_blob_u32(r, "doc.document")?),
            2 => {
                let count = r.get_u32("doc.count")? as usize;
                let mut docs = Vec::with_capacity(count);
                for _ in 0..count {
                    docs.push(get_blob_u32(r, "doc.document")?);
                }
                DocCommand::InsertMany(docs)
            }
            3 => DocCommand::Find {
                query: get_blob_u32(r, "doc.query")?,
                options: get_blob_u32(r, "doc.options")?,
            },
            4 => DocCommand::FindOne {
                query: get_blob_u32(r, "doc.query")?,
                options: get_blob_u32(r, "doc.options")?,
            },
            5 => DocCommand::UpdateOne {
                query: get_blob_u32(r, "doc.query")?,
                update: get_blob_u32(r, "doc.update")?,
                options: get_blob_u32(r, "doc.options")?,
            },
            6 => DocCommand::UpdateMany {
                query: get_blob_u32(r, "doc.query")?,
                update: get_blob_u32(r, "doc.update")?,
                options: get_blob_u32(r, "doc.options")?,
            },
            7 => DocCommand::DeleteOne {
                query: get_blob_u32(r, "doc.query")?,
                options: get_blob_u32(r, "doc.options")?,
            },
            8 => DocCommand::DeleteMany {
                query: get_blob_u32(r, "doc.query")?,
                options: get_blob_u32(r, "doc.options")?,
            },
            other => {
                return Err(ProtocolError::UnknownOpType {
                    family: "document",
                    value: other,
                });
            }
        })
    }
}

/// A document query filter on the wire — the structured form of a `{...}`
/// query. The server maps it to the document engine's own `Filter`; values
/// reuse the tagged [`Value`] encoding. This is what the `query` blob of a
/// [`DocCommand`] carries.
#[derive(Clone, PartialEq, Debug)]
pub enum DocQuery {
    /// Matches every document (the empty query `{}`).
    All,
    /// `field == value`.
    Eq(String, Value),
    /// `field != value`.
    Ne(String, Value),
    /// `field > value`.
    Gt(String, Value),
    /// `field < value`.
    Lt(String, Value),
    /// `field >= value`.
    Gte(String, Value),
    /// `field <= value`.
    Lte(String, Value),
    /// `field` is one of the values.
    In(String, Vec<Value>),
    /// `field` is none of the values.
    Nin(String, Vec<Value>),
    /// Whether `field` is present.
    Exists(String, bool),
    /// All sub-queries match.
    And(Vec<DocQuery>),
    /// Any sub-query matches.
    Or(Vec<DocQuery>),
    /// The sub-query does not match.
    Not(Box<DocQuery>),
}

impl DocQuery {
    /// Encode to a standalone byte string (the `query` blob of a command).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut w = Writer::new();
        self.encode(&mut w)?;
        Ok(w.into_vec())
    }

    /// Decode from a `query` blob, requiring all bytes to be consumed.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let q = Self::decode(&mut r)?;
        r.expect_end()?;
        Ok(q)
    }

    fn encode(&self, w: &mut Writer) -> Result<()> {
        match self {
            DocQuery::All => w.put_u8(0),
            DocQuery::Eq(f, v) => field_op(w, 1, f, v)?,
            DocQuery::Ne(f, v) => field_op(w, 2, f, v)?,
            DocQuery::Gt(f, v) => field_op(w, 3, f, v)?,
            DocQuery::Lt(f, v) => field_op(w, 4, f, v)?,
            DocQuery::Gte(f, v) => field_op(w, 5, f, v)?,
            DocQuery::Lte(f, v) => field_op(w, 6, f, v)?,
            DocQuery::In(f, set) => field_set(w, 7, f, set)?,
            DocQuery::Nin(f, set) => field_set(w, 8, f, set)?,
            DocQuery::Exists(f, want) => {
                w.put_u8(9);
                w.put_str_u16("query.field", f)?;
                w.put_u8(u8::from(*want));
            }
            DocQuery::And(subs) => group(w, 10, subs)?,
            DocQuery::Or(subs) => group(w, 11, subs)?,
            DocQuery::Not(inner) => {
                w.put_u8(12);
                inner.encode(w)?;
            }
        }
        Ok(())
    }

    fn decode(r: &mut Reader) -> Result<Self> {
        let tag = r.get_u8("query.tag")?;
        Ok(match tag {
            0 => DocQuery::All,
            1 => DocQuery::Eq(read_field(r)?, Value::decode_tagged(r)?),
            2 => DocQuery::Ne(read_field(r)?, Value::decode_tagged(r)?),
            3 => DocQuery::Gt(read_field(r)?, Value::decode_tagged(r)?),
            4 => DocQuery::Lt(read_field(r)?, Value::decode_tagged(r)?),
            5 => DocQuery::Gte(read_field(r)?, Value::decode_tagged(r)?),
            6 => DocQuery::Lte(read_field(r)?, Value::decode_tagged(r)?),
            7 => DocQuery::In(read_field(r)?, decode_set(r)?),
            8 => DocQuery::Nin(read_field(r)?, decode_set(r)?),
            9 => {
                let f = read_field(r)?;
                DocQuery::Exists(f, r.get_u8("query.exists")? != 0)
            }
            10 => DocQuery::And(decode_group(r)?),
            11 => DocQuery::Or(decode_group(r)?),
            12 => DocQuery::Not(Box::new(DocQuery::decode(r)?)),
            other => {
                return Err(ProtocolError::BadEnum {
                    field: "query.tag",
                    value: other,
                });
            }
        })
    }
}

fn field_op(w: &mut Writer, tag: u8, field: &str, v: &Value) -> Result<()> {
    w.put_u8(tag);
    w.put_str_u16("query.field", field)?;
    v.encode_tagged(w)
}

fn field_set(w: &mut Writer, tag: u8, field: &str, set: &[Value]) -> Result<()> {
    w.put_u8(tag);
    w.put_str_u16("query.field", field)?;
    let count: u32 = set
        .len()
        .try_into()
        .map_err(|_| ProtocolError::ValueTooLarge { field: "query.set" })?;
    w.put_u32(count);
    for v in set {
        v.encode_tagged(w)?;
    }
    Ok(())
}

fn group(w: &mut Writer, tag: u8, subs: &[DocQuery]) -> Result<()> {
    w.put_u8(tag);
    let count: u32 = subs
        .len()
        .try_into()
        .map_err(|_| ProtocolError::ValueTooLarge {
            field: "query.group",
        })?;
    w.put_u32(count);
    for s in subs {
        s.encode(w)?;
    }
    Ok(())
}

fn read_field(r: &mut Reader) -> Result<String> {
    r.get_str_u16("query.field")
}

fn decode_set(r: &mut Reader) -> Result<Vec<Value>> {
    let count = r.get_u32("query.set_count")? as usize;
    let mut out = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        out.push(Value::decode_tagged(r)?);
    }
    Ok(out)
}

fn decode_group(r: &mut Reader) -> Result<Vec<DocQuery>> {
    let count = r.get_u32("query.group_count")? as usize;
    let mut out = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        out.push(DocQuery::decode(r)?);
    }
    Ok(out)
}

/// The op-specific body of a [`crate::Message::KvOp`]. Keys and values are
/// opaque byte strings.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KvCommand {
    /// Get a key.
    Get {
        /// The key.
        key: Vec<u8>,
    },
    /// Put a key/value (upsert).
    Put {
        /// The key.
        key: Vec<u8>,
        /// The value.
        value: Vec<u8>,
    },
    /// Delete a key.
    Delete {
        /// The key.
        key: Vec<u8>,
    },
    /// Range scan `[start, end)` up to `max_results`.
    Range {
        /// Inclusive start key.
        start: Vec<u8>,
        /// Exclusive end key.
        end: Vec<u8>,
        /// Cap on returned entries.
        max_results: u32,
    },
    /// Prefix scan up to `max_results`.
    Scan {
        /// Key prefix.
        prefix: Vec<u8>,
        /// Cap on returned entries.
        max_results: u32,
    },
}

impl KvCommand {
    pub(crate) fn op_type(&self) -> u8 {
        match self {
            KvCommand::Get { .. } => 1,
            KvCommand::Put { .. } => 2,
            KvCommand::Delete { .. } => 3,
            KvCommand::Range { .. } => 4,
            KvCommand::Scan { .. } => 5,
        }
    }

    pub(crate) fn encode_body(&self, w: &mut Writer) -> Result<()> {
        match self {
            KvCommand::Get { key } | KvCommand::Delete { key } => w.put_bytes_u16("kv.key", key)?,
            KvCommand::Put { key, value } => {
                w.put_bytes_u16("kv.key", key)?;
                w.put_bytes_u32("kv.value", value)?;
            }
            KvCommand::Range {
                start,
                end,
                max_results,
            } => {
                w.put_bytes_u16("kv.start", start)?;
                w.put_bytes_u16("kv.end", end)?;
                w.put_u32(*max_results);
            }
            KvCommand::Scan {
                prefix,
                max_results,
            } => {
                w.put_bytes_u16("kv.prefix", prefix)?;
                w.put_u32(*max_results);
            }
        }
        Ok(())
    }

    pub(crate) fn decode_body(op_type: u8, r: &mut Reader) -> Result<Self> {
        Ok(match op_type {
            1 => KvCommand::Get {
                key: get_blob_u16(r, "kv.key")?,
            },
            2 => KvCommand::Put {
                key: get_blob_u16(r, "kv.key")?,
                value: get_blob_u32(r, "kv.value")?,
            },
            3 => KvCommand::Delete {
                key: get_blob_u16(r, "kv.key")?,
            },
            4 => KvCommand::Range {
                start: get_blob_u16(r, "kv.start")?,
                end: get_blob_u16(r, "kv.end")?,
                max_results: r.get_u32("kv.max_results")?,
            },
            5 => KvCommand::Scan {
                prefix: get_blob_u16(r, "kv.prefix")?,
                max_results: r.get_u32("kv.max_results")?,
            },
            other => {
                return Err(ProtocolError::UnknownOpType {
                    family: "kv",
                    value: other,
                });
            }
        })
    }
}

/// The op-specific body of a [`crate::Message::KvResult`]. The `op_type` is
/// echoed so a multiplexed client knows which request a reply belongs to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KvResultBody {
    /// Result of a `Get`: the value if found.
    Get {
        /// The value, or `None` if the key was absent/invisible.
        value: Option<Vec<u8>>,
    },
    /// Result of a `Put`.
    Put,
    /// Result of a `Delete`.
    Delete,
    /// Result of a `Range` scan.
    Range {
        /// Returned key/value entries.
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        /// Whether more result frames follow.
        more_frames: bool,
    },
    /// Result of a prefix `Scan`.
    Scan {
        /// Returned key/value entries.
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        /// Whether more result frames follow.
        more_frames: bool,
    },
}

impl KvResultBody {
    pub(crate) fn op_type(&self) -> u8 {
        match self {
            KvResultBody::Get { .. } => 1,
            KvResultBody::Put => 2,
            KvResultBody::Delete => 3,
            KvResultBody::Range { .. } => 4,
            KvResultBody::Scan { .. } => 5,
        }
    }

    pub(crate) fn encode_body(&self, w: &mut Writer) -> Result<()> {
        match self {
            KvResultBody::Get { value } => match value {
                Some(v) => {
                    w.put_u8(1);
                    w.put_bytes_u32("kv.value", v)?;
                }
                None => w.put_u8(0),
            },
            KvResultBody::Put | KvResultBody::Delete => {}
            KvResultBody::Range {
                entries,
                more_frames,
            }
            | KvResultBody::Scan {
                entries,
                more_frames,
            } => {
                encode_entries(entries, w)?;
                w.put_u8(u8::from(*more_frames));
            }
        }
        Ok(())
    }

    pub(crate) fn decode_body(op_type: u8, r: &mut Reader) -> Result<Self> {
        Ok(match op_type {
            1 => {
                let found = r.get_u8("kv.found")? != 0;
                KvResultBody::Get {
                    value: if found {
                        Some(get_blob_u32(r, "kv.value")?)
                    } else {
                        None
                    },
                }
            }
            2 => KvResultBody::Put,
            3 => KvResultBody::Delete,
            4 => {
                let entries = decode_entries(r)?;
                KvResultBody::Range {
                    entries,
                    more_frames: r.get_u8("kv.more_frames")? != 0,
                }
            }
            5 => {
                let entries = decode_entries(r)?;
                KvResultBody::Scan {
                    entries,
                    more_frames: r.get_u8("kv.more_frames")? != 0,
                }
            }
            other => {
                return Err(ProtocolError::UnknownOpType {
                    family: "kv-result",
                    value: other,
                });
            }
        })
    }
}

fn encode_entries(entries: &[(Vec<u8>, Vec<u8>)], w: &mut Writer) -> Result<()> {
    let count: u32 = entries
        .len()
        .try_into()
        .map_err(|_| ProtocolError::ValueTooLarge {
            field: "kv.entry_count",
        })?;
    w.put_u32(count);
    for (k, v) in entries {
        w.put_bytes_u16("kv.entry_key", k)?;
        w.put_bytes_u32("kv.entry_value", v)?;
    }
    Ok(())
}

fn decode_entries(r: &mut Reader) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let count = r.get_u32("kv.entry_count")? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let key = get_blob_u16(r, "kv.entry_key")?;
        let value = get_blob_u32(r, "kv.entry_value")?;
        entries.push((key, value));
    }
    Ok(entries)
}
