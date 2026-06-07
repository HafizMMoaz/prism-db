//! Length-prefixed framing.
//!
//! Every message on the wire is `[length: u32][payload: length bytes]`, where
//! `length` does not include the prefix itself (`docs/specs/wire-protocol.md`).
//! Little-endian, matching the rest of the protocol — the ADR's mention of a
//! big-endian prefix is superseded by the normative spec.
//!
//! These functions are pure buffer operations (no socket I/O): the server reads
//! bytes off the wire into a buffer and calls [`parse`]; it builds a reply with
//! [`encode`] and writes the bytes out.

use crate::MAX_FRAME_SIZE;
use crate::error::{ProtocolError, Result};

/// The size of the length prefix.
pub const LENGTH_PREFIX: usize = 4;

/// Wrap `payload` in a length-prefixed frame.
pub fn encode(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(LENGTH_PREFIX + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Try to parse one frame from the front of `buf`.
///
/// Returns:
/// - `Ok(Some((payload, consumed)))` — a complete frame; `payload` is the slice
///   between the prefix and the frame end, `consumed` is the total bytes to
///   advance the read buffer by (prefix + payload).
/// - `Ok(None)` — not enough bytes yet; the caller should read more and retry.
/// - `Err(FrameTooLarge)` — the declared length exceeds [`MAX_FRAME_SIZE`]; the
///   caller should close the connection.
pub fn parse(buf: &[u8]) -> Result<Option<(&[u8], usize)>> {
    if buf.len() < LENGTH_PREFIX {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge { len });
    }
    let end = LENGTH_PREFIX + len;
    if buf.len() < end {
        return Ok(None);
    }
    Ok(Some((&buf[LENGTH_PREFIX..end], end)))
}
