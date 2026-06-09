//! Little-endian read/write primitives for the wire format.
//!
//! Per `docs/specs/wire-protocol.md`, all multi-byte integers are little-endian.
//! [`Writer`] appends to an owned buffer (encoding is infallible for well-formed
//! messages, except length-prefix overflow); [`Reader`] is a bounds-checked
//! cursor over an untrusted byte slice.

use crate::error::{ProtocolError, Result};

/// A growable little-endian encoder.
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// A new empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// A writer with reserved capacity.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    /// Append a single byte.
    pub fn put_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    /// Append a little-endian `u16`.
    pub fn put_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Append a little-endian `u32`.
    pub fn put_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Append a little-endian `u64`.
    pub fn put_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Append a little-endian `u128`.
    pub fn put_u128(&mut self, v: u128) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Append raw bytes verbatim (no length prefix).
    pub fn put_raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Append `n` zero bytes (reserved/padding fields).
    pub fn put_zeros(&mut self, n: usize) {
        self.buf.resize(self.buf.len() + n, 0);
    }

    /// Append a `u16`-length-prefixed byte string.
    pub fn put_bytes_u16(&mut self, field: &'static str, bytes: &[u8]) -> Result<()> {
        let len: u16 = bytes
            .len()
            .try_into()
            .map_err(|_| ProtocolError::ValueTooLarge { field })?;
        self.put_u16(len);
        self.put_raw(bytes);
        Ok(())
    }

    /// Append a `u32`-length-prefixed byte string.
    pub fn put_bytes_u32(&mut self, field: &'static str, bytes: &[u8]) -> Result<()> {
        let len: u32 = bytes
            .len()
            .try_into()
            .map_err(|_| ProtocolError::ValueTooLarge { field })?;
        self.put_u32(len);
        self.put_raw(bytes);
        Ok(())
    }

    /// Append a `u16`-length-prefixed UTF-8 string.
    pub fn put_str_u16(&mut self, field: &'static str, s: &str) -> Result<()> {
        self.put_bytes_u16(field, s.as_bytes())
    }

    /// Append a `u32`-length-prefixed UTF-8 string.
    pub fn put_str_u32(&mut self, field: &'static str, s: &str) -> Result<()> {
        self.put_bytes_u32(field, s.as_bytes())
    }

    /// Consume the writer, returning the encoded bytes.
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    /// The bytes written so far.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }
}

/// A bounds-checked little-endian decoder over a byte slice.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// A reader positioned at the start of `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether the whole buffer has been consumed.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn take(&mut self, n: usize, needed: &'static str) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or(ProtocolError::Truncated { needed })?;
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Read a single byte.
    pub fn get_u8(&mut self, needed: &'static str) -> Result<u8> {
        Ok(self.take(1, needed)?[0])
    }

    /// Read a little-endian `u16`.
    pub fn get_u16(&mut self, needed: &'static str) -> Result<u16> {
        let b = self.take(2, needed)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Read a little-endian `u32`.
    pub fn get_u32(&mut self, needed: &'static str) -> Result<u32> {
        let b = self.take(4, needed)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a little-endian `u64`.
    pub fn get_u64(&mut self, needed: &'static str) -> Result<u64> {
        let b = self.take(8, needed)?;
        Ok(u64::from_le_bytes(b.try_into().expect("8 bytes")))
    }

    /// Read a little-endian `u128`.
    pub fn get_u128(&mut self, needed: &'static str) -> Result<u128> {
        let b = self.take(16, needed)?;
        Ok(u128::from_le_bytes(b.try_into().expect("16 bytes")))
    }

    /// Read `n` raw bytes.
    pub fn get_raw(&mut self, n: usize, needed: &'static str) -> Result<&'a [u8]> {
        self.take(n, needed)
    }

    /// Skip `n` bytes (reserved/padding fields).
    pub fn skip(&mut self, n: usize, needed: &'static str) -> Result<()> {
        self.take(n, needed).map(|_| ())
    }

    /// Read a fixed-size array of `N` bytes.
    pub fn get_array<const N: usize>(&mut self, needed: &'static str) -> Result<[u8; N]> {
        Ok(self.take(N, needed)?.try_into().expect("N bytes"))
    }

    /// Read a `u16`-length-prefixed byte string.
    pub fn get_bytes_u16(&mut self, needed: &'static str) -> Result<&'a [u8]> {
        let len = self.get_u16(needed)? as usize;
        self.take(len, needed)
    }

    /// Read a `u32`-length-prefixed byte string.
    pub fn get_bytes_u32(&mut self, needed: &'static str) -> Result<&'a [u8]> {
        let len = self.get_u32(needed)? as usize;
        self.take(len, needed)
    }

    /// Read a `u16`-length-prefixed UTF-8 string.
    pub fn get_str_u16(&mut self, field: &'static str) -> Result<String> {
        let bytes = self.get_bytes_u16(field)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| ProtocolError::BadUtf8 { field })
    }

    /// Read a `u32`-length-prefixed UTF-8 string.
    pub fn get_str_u32(&mut self, field: &'static str) -> Result<String> {
        let bytes = self.get_bytes_u32(field)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| ProtocolError::BadUtf8 { field })
    }

    /// Error unless the whole buffer was consumed (catches malformed messages
    /// with trailing junk).
    pub fn expect_end(&self) -> Result<()> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(ProtocolError::TrailingBytes {
                count: self.remaining(),
            })
        }
    }
}
