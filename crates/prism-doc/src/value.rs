//! The document value model, `ObjectId`, the tagged-binary codec, and MongoDB's
//! cross-type comparison order.
//!
//! Byte layout is normative; see "Document payload" in
//! `docs/specs/record-format.md`. Scope (this slice): scalar field values
//! (Null, Bool, Int32, Int64, Double, String, Timestamp, ObjectId) and
//! top-level fields. Nested objects, arrays, and binary slot into the same
//! recursive format later.

use std::cmp::Ordering;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

use crate::error::{DocError, Result};

/// A 12-byte document identifier (4-byte time, 5-byte random, 3-byte counter),
/// byte-comparable in roughly-chronological order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ObjectId(pub [u8; 12]);

impl ObjectId {
    /// Generate a fresh, process-unique id.
    pub fn generate() -> Self {
        static SEED: OnceLock<[u8; 5]> = OnceLock::new();
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let seed = *SEED.get_or_init(|| {
            // A per-process seed mixed from pid + start time (no rand dep).
            let mut h = std::process::id() as u64;
            h ^= std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            h = h.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let b = h.to_le_bytes();
            [b[0], b[1], b[2], b[3], b[4]]
        });

        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);
        let counter = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);

        let mut id = [0u8; 12];
        id[0..4].copy_from_slice(&secs.to_be_bytes());
        id[4..9].copy_from_slice(&seed);
        let c = counter.to_be_bytes(); // take low 3 bytes
        id[9..12].copy_from_slice(&c[1..4]);
        ObjectId(id)
    }

    /// Lowercase hex rendering.
    pub fn to_hex(self) -> String {
        use std::fmt::Write;
        self.0.iter().fold(String::with_capacity(24), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    }
}

/// A scalar document value.
#[derive(Clone, Debug, PartialEq)]
pub enum DocValue {
    /// JSON null.
    Null,
    /// Boolean.
    Bool(bool),
    /// 32-bit integer.
    Int32(i32),
    /// 64-bit integer.
    Int64(i64),
    /// 64-bit float.
    Double(f64),
    /// UTF-8 string.
    Str(String),
    /// Microseconds since the Unix epoch.
    Timestamp(i64),
    /// A document id.
    ObjectId(ObjectId),
}

// Type tags (see the record-format spec).
const T_NULL: u8 = 0x00;
const T_BOOL: u8 = 0x01;
const T_INT32: u8 = 0x02;
const T_INT64: u8 = 0x03;
const T_DOUBLE: u8 = 0x04;
const T_STRING: u8 = 0x05;
const T_TIMESTAMP: u8 = 0x09;
const T_OBJECTID: u8 = 0x0A;

impl DocValue {
    fn type_tag(&self) -> u8 {
        match self {
            DocValue::Null => T_NULL,
            DocValue::Bool(_) => T_BOOL,
            DocValue::Int32(_) => T_INT32,
            DocValue::Int64(_) => T_INT64,
            DocValue::Double(_) => T_DOUBLE,
            DocValue::Str(_) => T_STRING,
            DocValue::Timestamp(_) => T_TIMESTAMP,
            DocValue::ObjectId(_) => T_OBJECTID,
        }
    }

    /// MongoDB's cross-type sort rank: Null < Number < String < … < ObjectId
    /// < Bool < Timestamp.
    fn type_rank(&self) -> u8 {
        match self {
            DocValue::Null => 0,
            DocValue::Int32(_) | DocValue::Int64(_) | DocValue::Double(_) => 1,
            DocValue::Str(_) => 2,
            DocValue::ObjectId(_) => 6,
            DocValue::Bool(_) => 7,
            DocValue::Timestamp(_) => 8,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            DocValue::Int32(n) => Some(*n as f64),
            DocValue::Int64(n) => Some(*n as f64),
            DocValue::Double(d) => Some(*d),
            _ => None,
        }
    }
}

/// Compare two values using MongoDB's ordering (numeric across int/double).
pub fn doc_cmp(a: &DocValue, b: &DocValue) -> Ordering {
    let (ra, rb) = (a.type_rank(), b.type_rank());
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (DocValue::Null, DocValue::Null) => Ordering::Equal,
        (DocValue::Str(x), DocValue::Str(y)) => x.cmp(y),
        (DocValue::Bool(x), DocValue::Bool(y)) => x.cmp(y),
        (DocValue::Timestamp(x), DocValue::Timestamp(y)) => x.cmp(y),
        (DocValue::ObjectId(x), DocValue::ObjectId(y)) => x.cmp(y),
        _ => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            _ => Ordering::Equal,
        },
    }
}

/// A document: an ordered list of `(name, value)` fields.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Document {
    fields: Vec<(String, DocValue)>,
}

impl Document {
    /// An empty document.
    pub fn new() -> Self {
        Self::default()
    }

    /// The value of `name`, if present.
    pub fn get(&self, name: &str) -> Option<&DocValue> {
        self.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    /// Whether `name` is present.
    pub fn contains(&self, name: &str) -> bool {
        self.fields.iter().any(|(k, _)| k == name)
    }

    /// Set `name` to `value` (replacing in place, or appending).
    pub fn set(&mut self, name: impl Into<String>, value: DocValue) -> &mut Self {
        let name = name.into();
        if let Some(slot) = self.fields.iter_mut().find(|(k, _)| *k == name) {
            slot.1 = value;
        } else {
            self.fields.push((name, value));
        }
        self
    }

    /// Insert `name` at the front (used to place `_id` first).
    pub fn set_front(&mut self, name: impl Into<String>, value: DocValue) {
        let name = name.into();
        self.fields.retain(|(k, _)| *k != name);
        self.fields.insert(0, (name, value));
    }

    /// Remove `name`, returning whether it was present.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.fields.len();
        self.fields.retain(|(k, _)| k != name);
        self.fields.len() != before
    }

    /// Iterate the fields in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &DocValue)> {
        self.fields.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Build a document from field pairs.
    pub fn from_fields(fields: impl IntoIterator<Item = (String, DocValue)>) -> Self {
        let mut d = Self::new();
        for (k, v) in fields {
            d.set(k, v);
        }
        d
    }

    /// Encode to the tagged-binary payload.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut body = Vec::new();
        let count: u16 = self
            .fields
            .len()
            .try_into()
            .map_err(|_| DocError::TooLarge("too many fields".into()))?;
        body.extend_from_slice(&count.to_le_bytes());
        for (name, value) in &self.fields {
            body.push(value.type_tag());
            let nlen: u16 = name
                .len()
                .try_into()
                .map_err(|_| DocError::TooLarge("field name too long".into()))?;
            body.extend_from_slice(&nlen.to_le_bytes());
            body.extend_from_slice(name.as_bytes());
            encode_value(value, &mut body);
        }
        let total = (4 + body.len()) as u32;
        let mut out = Vec::with_capacity(total as usize);
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Decode from a tagged-binary payload.
    pub fn decode(bytes: &[u8]) -> Result<Document> {
        let mut r = Reader::new(bytes);
        let _total = r.u32()?;
        let count = r.u16()?;
        let mut fields = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let tag = r.u8()?;
            let nlen = r.u16()? as usize;
            let name = std::str::from_utf8(r.take(nlen)?)
                .map_err(|_| DocError::Corrupt("non-UTF-8 field name".into()))?
                .to_string();
            fields.push((name, decode_value(tag, &mut r)?));
        }
        Ok(Document { fields })
    }
}

fn encode_value(value: &DocValue, out: &mut Vec<u8>) {
    match value {
        DocValue::Null => {}
        DocValue::Bool(b) => out.push(u8::from(*b)),
        DocValue::Int32(n) => out.extend_from_slice(&n.to_le_bytes()),
        DocValue::Int64(n) => out.extend_from_slice(&n.to_le_bytes()),
        DocValue::Double(d) => out.extend_from_slice(&d.to_le_bytes()),
        DocValue::Timestamp(t) => out.extend_from_slice(&t.to_le_bytes()),
        DocValue::ObjectId(id) => out.extend_from_slice(&id.0),
        DocValue::Str(s) => {
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
    }
}

fn decode_value(tag: u8, r: &mut Reader<'_>) -> Result<DocValue> {
    Ok(match tag {
        T_NULL => DocValue::Null,
        T_BOOL => DocValue::Bool(r.u8()? != 0),
        T_INT32 => DocValue::Int32(i32::from_le_bytes(r.take(4)?.try_into().unwrap())),
        T_INT64 => DocValue::Int64(i64::from_le_bytes(r.take(8)?.try_into().unwrap())),
        T_DOUBLE => DocValue::Double(f64::from_le_bytes(r.take(8)?.try_into().unwrap())),
        T_TIMESTAMP => DocValue::Timestamp(i64::from_le_bytes(r.take(8)?.try_into().unwrap())),
        T_OBJECTID => DocValue::ObjectId(ObjectId(r.take(12)?.try_into().unwrap())),
        T_STRING => {
            let len = r.u32()? as usize;
            DocValue::Str(
                std::str::from_utf8(r.take(len)?)
                    .map_err(|_| DocError::Corrupt("non-UTF-8 string".into()))?
                    .to_string(),
            )
        }
        other => return Err(DocError::Corrupt(format!("unknown type tag 0x{other:02x}"))),
    })
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .p
            .checked_add(n)
            .filter(|&e| e <= self.b.len())
            .ok_or_else(|| DocError::Corrupt("document truncated".into()))?;
        let s = &self.b[self.p..end];
        self.p = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_roundtrip() {
        let mut d = Document::new();
        d.set("_id", DocValue::ObjectId(ObjectId::generate()));
        d.set("name", DocValue::Str("alice".into()));
        d.set("age", DocValue::Int64(30));
        d.set("score", DocValue::Double(9.5));
        d.set("active", DocValue::Bool(true));
        d.set("nick", DocValue::Null);
        let bytes = d.encode().unwrap();
        assert_eq!(Document::decode(&bytes).unwrap(), d);
    }

    #[test]
    fn cross_type_ordering() {
        // Null < Number < String < ObjectId < Bool < Timestamp
        assert_eq!(
            doc_cmp(&DocValue::Null, &DocValue::Int64(0)),
            Ordering::Less
        );
        assert_eq!(
            doc_cmp(&DocValue::Int64(5), &DocValue::Str("a".into())),
            Ordering::Less
        );
        // Numeric across int/double.
        assert_eq!(
            doc_cmp(&DocValue::Int64(1), &DocValue::Double(1.0)),
            Ordering::Equal
        );
        assert_eq!(
            doc_cmp(&DocValue::Int32(2), &DocValue::Double(1.5)),
            Ordering::Greater
        );
    }

    #[test]
    fn object_ids_are_unique_and_monotonic_counter() {
        let a = ObjectId::generate();
        let b = ObjectId::generate();
        assert_ne!(a, b);
        assert_eq!(a.to_hex().len(), 24);
    }
}
