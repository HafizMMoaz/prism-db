//! The `prism-protocol` decode error type.
//!
//! Encoding never fails for well-formed in-memory messages, so only decoding
//! produces errors — a frame from the wire is untrusted input.

use std::fmt;

/// Convenience alias for results in this crate.
pub type Result<T> = std::result::Result<T, ProtocolError>;

/// An error decoding a frame or message from the wire.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ProtocolError {
    /// The buffer ended before a field could be fully read.
    Truncated {
        /// What was being read when the buffer ran out.
        needed: &'static str,
    },
    /// A length-prefixed string was not valid UTF-8.
    BadUtf8 {
        /// The field that failed to decode.
        field: &'static str,
    },
    /// The message-type byte in the header is not one we know.
    UnknownMessageType(u8),
    /// A value type tag is not one this protocol version encodes on the wire
    /// (e.g. nested Array/Object, which travel inside opaque document bytes).
    UnknownValueType(u8),
    /// An op-type byte (document or KV op) is out of range.
    UnknownOpType {
        /// Which op family the byte belonged to.
        family: &'static str,
        /// The offending value.
        value: u8,
    },
    /// An enum-like discriminant byte was out of range.
    BadEnum {
        /// The field that held the bad discriminant.
        field: &'static str,
        /// The offending value.
        value: u8,
    },
    /// A frame's declared length exceeds [`crate::MAX_FRAME_SIZE`].
    FrameTooLarge {
        /// The declared payload length.
        len: usize,
    },
    /// Trailing bytes remained after a message was fully decoded.
    TrailingBytes {
        /// How many bytes were left over.
        count: usize,
    },
    /// A value that should fit a length prefix was too large to encode.
    ValueTooLarge {
        /// The field that overflowed its length prefix.
        field: &'static str,
    },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::Truncated { needed } => {
                write!(f, "frame truncated while reading {needed}")
            }
            ProtocolError::BadUtf8 { field } => write!(f, "invalid UTF-8 in field {field}"),
            ProtocolError::UnknownMessageType(t) => write!(f, "unknown message type 0x{t:02x}"),
            ProtocolError::UnknownValueType(t) => write!(f, "unknown value type tag 0x{t:02x}"),
            ProtocolError::UnknownOpType { family, value } => {
                write!(f, "unknown {family} op type {value}")
            }
            ProtocolError::BadEnum { field, value } => {
                write!(f, "invalid value {value} for field {field}")
            }
            ProtocolError::FrameTooLarge { len } => {
                write!(f, "frame length {len} exceeds the maximum")
            }
            ProtocolError::TrailingBytes { count } => {
                write!(f, "{count} trailing bytes after message")
            }
            ProtocolError::ValueTooLarge { field } => {
                write!(f, "value for field {field} is too large to encode")
            }
        }
    }
}

impl std::error::Error for ProtocolError {}
