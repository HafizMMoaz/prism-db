//! The SQL type system and the relational tuple (row) encoding.
//!
//! Row layout is normative; see "Relational payload" in
//! `docs/specs/record-format.md`:
//! `[null_bitmap][fixed-width columns][var offset array][var data]`.
//! This is the payload the record store stores; the 24-byte MVCC header is added
//! by the record store, not here.
//!
//! Scope (this slice): `Bool`, `Int64`, `Text`. More types (`Int32`, floats,
//! `Timestamp`, `Blob`) slot into the same layout later.

use crate::error::{Result, SqlError};

/// A SQL column type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Type {
    /// Boolean.
    Bool,
    /// 64-bit signed integer.
    Int64,
    /// UTF-8 text.
    Text,
}

impl Type {
    /// The fixed width in bytes, or `None` for variable-width types.
    pub fn fixed_width(self) -> Option<usize> {
        match self {
            Type::Bool => Some(1),
            Type::Int64 => Some(8),
            Type::Text => None,
        }
    }
}

/// A SQL value.
#[derive(Clone, PartialEq, Debug)]
pub enum Value {
    /// SQL NULL.
    Null,
    /// Boolean.
    Bool(bool),
    /// 64-bit integer.
    Int64(i64),
    /// UTF-8 text.
    Text(String),
}

impl Value {
    /// Whether this value matches `ty` (NULL matches any type).
    pub fn type_matches(&self, ty: Type) -> bool {
        matches!(
            (self, ty),
            (Value::Null, _)
                | (Value::Bool(_), Type::Bool)
                | (Value::Int64(_), Type::Int64)
                | (Value::Text(_), Type::Text)
        )
    }

    /// Render for result display.
    pub fn display(&self) -> String {
        match self {
            Value::Null => "NULL".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Int64(n) => n.to_string(),
            Value::Text(s) => s.clone(),
        }
    }
}

/// Encode a row of `values` against `types` into the relational payload format.
pub fn encode_row(types: &[Type], values: &[Value]) -> Result<Vec<u8>> {
    if types.len() != values.len() {
        return Err(SqlError::Type(format!(
            "row has {} values for {} columns",
            values.len(),
            types.len()
        )));
    }
    let n = types.len();
    let nb_len = n.div_ceil(8);

    let mut null_bitmap = vec![0u8; nb_len];
    let mut fixed = Vec::new();
    let var_cols: Vec<usize> = (0..n)
        .filter(|&i| types[i].fixed_width().is_none())
        .collect();

    for (i, (&ty, value)) in types.iter().zip(values).enumerate() {
        if !value.type_matches(ty) {
            return Err(SqlError::Type(format!(
                "value {value:?} does not match {ty:?}"
            )));
        }
        if matches!(value, Value::Null) {
            null_bitmap[i / 8] |= 1 << (i % 8);
            continue;
        }
        if let Some(width) = ty.fixed_width() {
            let before = fixed.len();
            encode_fixed(value, &mut fixed);
            debug_assert_eq!(fixed.len() - before, width);
        }
    }

    let offset_array_len = var_cols.len() * 2;
    let var_data_start = nb_len + fixed.len() + offset_array_len;

    let mut offsets = Vec::with_capacity(var_cols.len());
    let mut var_data = Vec::new();
    for &i in &var_cols {
        offsets.push((var_data_start + var_data.len()) as u16);
        if let Value::Text(s) = &values[i] {
            var_data.extend_from_slice(s.as_bytes());
        }
        // NULL var columns contribute no bytes; their offset == the next one.
    }

    let mut out = Vec::with_capacity(var_data_start + var_data.len());
    out.extend_from_slice(&null_bitmap);
    out.extend_from_slice(&fixed);
    for off in offsets {
        out.extend_from_slice(&off.to_le_bytes());
    }
    out.extend_from_slice(&var_data);
    Ok(out)
}

/// Decode a row from a relational payload against `types`.
pub fn decode_row(types: &[Type], bytes: &[u8]) -> Result<Vec<Value>> {
    let n = types.len();
    let nb_len = n.div_ceil(8);
    if bytes.len() < nb_len {
        return Err(SqlError::Corrupt("row shorter than null bitmap".into()));
    }
    let is_null = |i: usize| bytes[i / 8] & (1 << (i % 8)) != 0;

    let mut values = vec![Value::Null; n];

    // Fixed-width columns, in order.
    let mut cursor = nb_len;
    for (i, &ty) in types.iter().enumerate() {
        if is_null(i) {
            continue;
        }
        if let Some(width) = ty.fixed_width() {
            let end = cursor + width;
            let slice = bytes
                .get(cursor..end)
                .ok_or_else(|| SqlError::Corrupt("fixed column out of bounds".into()))?;
            values[i] = decode_fixed(ty, slice)?;
            cursor = end;
        }
    }

    // Variable-width columns via the offset array.
    let var_cols: Vec<usize> = (0..n)
        .filter(|&i| types[i].fixed_width().is_none())
        .collect();
    let offset_array_start = cursor;
    let mut offsets = Vec::with_capacity(var_cols.len());
    for j in 0..var_cols.len() {
        let p = offset_array_start + j * 2;
        let raw = bytes
            .get(p..p + 2)
            .ok_or_else(|| SqlError::Corrupt("offset array out of bounds".into()))?;
        offsets.push(u16::from_le_bytes([raw[0], raw[1]]) as usize);
    }
    for (j, &i) in var_cols.iter().enumerate() {
        if is_null(i) {
            continue;
        }
        let start = offsets[j];
        let end = offsets.get(j + 1).copied().unwrap_or(bytes.len());
        let slice = bytes
            .get(start..end)
            .ok_or_else(|| SqlError::Corrupt("var column out of bounds".into()))?;
        values[i] = decode_var(types[i], slice)?;
    }

    Ok(values)
}

fn encode_fixed(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Bool(b) => out.push(u8::from(*b)),
        Value::Int64(n) => out.extend_from_slice(&n.to_le_bytes()),
        _ => unreachable!("encode_fixed called on a non-fixed value"),
    }
}

fn decode_fixed(ty: Type, slice: &[u8]) -> Result<Value> {
    Ok(match ty {
        Type::Bool => Value::Bool(slice[0] != 0),
        Type::Int64 => Value::Int64(i64::from_le_bytes(
            slice
                .try_into()
                .map_err(|_| SqlError::Corrupt("bad int64".into()))?,
        )),
        Type::Text => unreachable!("decode_fixed called on Text"),
    })
}

fn decode_var(ty: Type, slice: &[u8]) -> Result<Value> {
    match ty {
        Type::Text => Ok(Value::Text(
            std::str::from_utf8(slice)
                .map_err(|_| SqlError::Corrupt("invalid UTF-8 in text".into()))?
                .to_string(),
        )),
        _ => unreachable!("decode_var called on a fixed type"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn roundtrip_mixed_row() {
        let types = [Type::Int64, Type::Text, Type::Bool, Type::Text];
        let row = vec![
            Value::Int64(42),
            Value::Text("hello".into()),
            Value::Bool(true),
            Value::Text("".into()),
        ];
        let bytes = encode_row(&types, &row).unwrap();
        assert_eq!(decode_row(&types, &bytes).unwrap(), row);
    }

    #[test]
    fn roundtrip_with_nulls() {
        let types = [Type::Int64, Type::Text, Type::Bool];
        let row = vec![Value::Null, Value::Text("x".into()), Value::Null];
        let bytes = encode_row(&types, &row).unwrap();
        assert_eq!(decode_row(&types, &bytes).unwrap(), row);

        let row2 = vec![Value::Int64(7), Value::Null, Value::Bool(false)];
        let bytes2 = encode_row(&types, &row2).unwrap();
        assert_eq!(decode_row(&types, &bytes2).unwrap(), row2);
    }

    #[test]
    fn type_mismatch_is_rejected() {
        let types = [Type::Int64];
        assert!(encode_row(&types, &[Value::Text("nope".into())]).is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_rows_roundtrip(
            cols in proptest::collection::vec(0u8..3, 1..8),
            seed in any::<u64>(),
        ) {
            // Build a schema from the column-kind codes and a deterministic row.
            let types: Vec<Type> = cols.iter().map(|c| match c {
                0 => Type::Bool,
                1 => Type::Int64,
                _ => Type::Text,
            }).collect();
            let mut s = seed;
            let mut next = || { s = s.wrapping_mul(6364136223846793005).wrapping_add(1); s };
            let row: Vec<Value> = types.iter().map(|t| {
                if next() % 5 == 0 {
                    Value::Null
                } else {
                    match t {
                        Type::Bool => Value::Bool(next() % 2 == 0),
                        Type::Int64 => Value::Int64(next() as i64),
                        Type::Text => Value::Text(format!("v{}", next() % 1000)),
                    }
                }
            }).collect();
            let bytes = encode_row(&types, &row).unwrap();
            prop_assert_eq!(decode_row(&types, &bytes).unwrap(), row);
        }
    }
}
