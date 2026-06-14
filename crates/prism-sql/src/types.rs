//! The SQL type system and the relational tuple (row) encoding.
//!
//! Row layout is normative; see "Relational payload" in
//! `docs/specs/record-format.md`:
//! `[null_bitmap][fixed-width columns][var offset array][var data]`.
//! This is the payload the record store stores; the 24-byte MVCC header is added
//! by the record store, not here.
//!
//! Scope (this slice): `Bool`, `Int64`, `Double`, `Text`. More types (`Int32`,
//! `Timestamp`, `Blob`) slot into the same layout later.

use std::hash::{Hash, Hasher};

use crate::error::{Result, SqlError};

/// A SQL column type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Type {
    /// Boolean.
    Bool,
    /// 64-bit signed integer.
    Int64,
    /// 64-bit IEEE-754 floating point.
    Double,
    /// A point in time, stored as microseconds since the Unix epoch (UTC).
    Timestamp,
    /// UTF-8 text.
    Text,
}

impl Type {
    /// The fixed width in bytes, or `None` for variable-width types.
    pub fn fixed_width(self) -> Option<usize> {
        match self {
            Type::Bool => Some(1),
            Type::Int64 => Some(8),
            Type::Double => Some(8),
            Type::Timestamp => Some(8),
            Type::Text => None,
        }
    }
}

/// A SQL value.
///
/// `PartialEq`/`Eq`/`Hash` are implemented by hand because `f64` is neither
/// `Eq` nor `Hash`; `Double` compares and hashes by its bit pattern, which gives
/// a total, hashable equality suitable for `DISTINCT` / `GROUP BY` keys. SQL's
/// numeric comparison operators go through `compare`/`value_cmp`, not this.
#[derive(Clone, Debug)]
pub enum Value {
    /// SQL NULL.
    Null,
    /// Boolean.
    Bool(bool),
    /// 64-bit integer.
    Int64(i64),
    /// 64-bit float.
    Double(f64),
    /// Microseconds since the Unix epoch (UTC).
    Timestamp(i64),
    /// UTF-8 text.
    Text(String),
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int64(a), Value::Int64(b)) => a == b,
            (Value::Double(a), Value::Double(b)) => a.to_bits() == b.to_bits(),
            (Value::Timestamp(a), Value::Timestamp(b)) => a == b,
            (Value::Text(a), Value::Text(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Value::Null => 0u8.hash(state),
            Value::Bool(b) => (1u8, b).hash(state),
            Value::Int64(n) => (2u8, n).hash(state),
            Value::Double(d) => (3u8, d.to_bits()).hash(state),
            Value::Timestamp(t) => (5u8, t).hash(state),
            Value::Text(s) => (4u8, s).hash(state),
        }
    }
}

impl Value {
    /// Whether this value matches `ty` (NULL matches any type).
    pub fn type_matches(&self, ty: Type) -> bool {
        matches!(
            (self, ty),
            (Value::Null, _)
                | (Value::Bool(_), Type::Bool)
                | (Value::Int64(_), Type::Int64)
                | (Value::Double(_), Type::Double)
                | (Value::Timestamp(_), Type::Timestamp)
                | (Value::Text(_), Type::Text)
        )
    }

    /// Render for result display.
    pub fn display(&self) -> String {
        match self {
            Value::Null => "NULL".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Int64(n) => n.to_string(),
            Value::Double(d) => format_double(*d),
            Value::Timestamp(t) => format_timestamp(*t),
            Value::Text(s) => s.clone(),
        }
    }
}

/// Render a double so whole values keep a trailing `.0` (e.g. `2` -> `2.0`),
/// distinguishing them from integers in result output.
pub(crate) fn format_double(d: f64) -> String {
    if d.is_finite() && d == d.trunc() {
        format!("{d:.1}")
    } else {
        d.to_string()
    }
}

const MICROS_PER_SEC: i64 = 1_000_000;
const SECS_PER_DAY: i64 = 86_400;

/// Days since 1970-01-01 for a civil (proleptic Gregorian) date.
/// Howard Hinnant's `days_from_civil`.
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// The civil `(year, month, day)` for a count of days since 1970-01-01.
pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format epoch microseconds as `YYYY-MM-DD HH:MM:SS` (UTC), adding `.ffffff`
/// when there is a sub-second component.
pub(crate) fn format_timestamp(micros: i64) -> String {
    let secs = micros.div_euclid(MICROS_PER_SEC);
    let frac = micros.rem_euclid(MICROS_PER_SEC);
    let (y, mo, d) = civil_from_days(secs.div_euclid(SECS_PER_DAY));
    let rem = secs.rem_euclid(SECS_PER_DAY);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let base = format!("{y:04}-{mo:02}-{d:02} {hh:02}:{mm:02}:{ss:02}");
    if frac == 0 {
        base
    } else {
        format!("{base}.{frac:06}")
    }
}

/// Parse `YYYY-MM-DD[ HH:MM[:SS[.ffffff]]]` (a space or `T` separates the time)
/// into epoch microseconds (UTC).
pub(crate) fn parse_timestamp(s: &str) -> Result<i64> {
    let s = s.trim();
    let err = || SqlError::Type(format!("invalid timestamp: {s:?}"));
    let (date, time) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut dp = date.split('-');
    let y: i64 = dp.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    let mo: i64 = dp.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    let d: i64 = dp.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    if dp.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return Err(err());
    }

    let (mut hh, mut mm, mut ss, mut frac) = (0i64, 0i64, 0i64, 0i64);
    if let Some(time) = time {
        let (clock, fraction) = match time.split_once('.') {
            Some((c, f)) => (c, Some(f)),
            None => (time, None),
        };
        let mut tp = clock.split(':');
        hh = tp.next().ok_or_else(err)?.parse().map_err(|_| err())?;
        mm = tp.next().ok_or_else(err)?.parse().map_err(|_| err())?;
        ss = match tp.next() {
            Some(v) => v.parse().map_err(|_| err())?,
            None => 0,
        };
        if tp.next().is_some() {
            return Err(err());
        }
        if let Some(f) = fraction {
            // Pad/truncate the fractional part to exactly six digits (micros).
            let digits: String = f.chars().take(6).collect();
            if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
                return Err(err());
            }
            let padded = format!("{digits:0<6}");
            frac = padded.parse().map_err(|_| err())?;
        }
    }
    if !(0..=23).contains(&hh) || !(0..=59).contains(&mm) || !(0..=60).contains(&ss) {
        return Err(err());
    }
    let days = days_from_civil(y, mo, d);
    let secs = days * SECS_PER_DAY + hh * 3600 + mm * 60 + ss;
    Ok(secs * MICROS_PER_SEC + frac)
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
        // Coerce literals into a column's type: an integer widens to a DOUBLE
        // (`… VALUES (5)`); for a TIMESTAMP column a string is parsed as a
        // datetime and an integer is taken as raw epoch microseconds.
        let coerced;
        let value = match (ty, value) {
            (Type::Double, Value::Int64(n)) => {
                coerced = Value::Double(*n as f64);
                &coerced
            }
            (Type::Timestamp, Value::Text(s)) => {
                coerced = Value::Timestamp(parse_timestamp(s)?);
                &coerced
            }
            (Type::Timestamp, Value::Int64(n)) => {
                coerced = Value::Timestamp(*n);
                &coerced
            }
            _ => value,
        };
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
        Value::Double(d) => out.extend_from_slice(&d.to_le_bytes()),
        Value::Timestamp(t) => out.extend_from_slice(&t.to_le_bytes()),
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
        Type::Double => Value::Double(f64::from_le_bytes(
            slice
                .try_into()
                .map_err(|_| SqlError::Corrupt("bad double".into()))?,
        )),
        Type::Timestamp => Value::Timestamp(i64::from_le_bytes(
            slice
                .try_into()
                .map_err(|_| SqlError::Corrupt("bad timestamp".into()))?,
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
            cols in proptest::collection::vec(0u8..5, 1..8),
            seed in any::<u64>(),
        ) {
            // Build a schema from the column-kind codes and a deterministic row.
            let types: Vec<Type> = cols.iter().map(|c| match c {
                0 => Type::Bool,
                1 => Type::Int64,
                2 => Type::Double,
                3 => Type::Timestamp,
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
                        Type::Double => Value::Double((next() % 1_000_000) as f64 / 7.0),
                        Type::Timestamp => Value::Timestamp(next() as i64),
                        Type::Text => Value::Text(format!("v{}", next() % 1000)),
                    }
                }
            }).collect();
            let bytes = encode_row(&types, &row).unwrap();
            prop_assert_eq!(decode_row(&types, &bytes).unwrap(), row);
        }
    }
}
