//! `prism-protocol` — wire protocol types and serialization.
//!
//! Message enums and their stable little-endian binary encoding, plus the
//! length-prefixed framing — pure data definitions with no socket I/O, so both
//! the server and every client depend on it without pulling in the engine. See
//! `docs/specs/wire-protocol.md` and `docs/adr/0008-binary-wire-protocol.md`.
//!
//! The on-wire shape is `[length: u32][payload]`, where the payload is a 12-byte
//! common header (message type + `request_id`) followed by a message-specific
//! body. [`Packet`] is the header + body; [`frame`] adds and strips the length
//! prefix.
//!
//! **Scope (this slice):** the session and transaction control plane — the
//! handshake, authentication, `Begin`/`Commit`/`Abort`, cancellation, notices,
//! and keep-alive — plus framing, the little-endian codec, and the error
//! trailer. The query data plane (`SqlExecute`/`DocOp`/`KvOp` and their results)
//! is the next increment.

pub mod codec;
pub mod error;
pub mod frame;
pub mod message;

pub use error::{ProtocolError, Result};
pub use message::{
    AuthMechanism, ErrorInfo, Message, MessageType, NoticeSeverity, Packet, TxnMode,
};

/// The protocol version carried in `Hello` (`docs/specs/wire-protocol.md`).
pub const PROTOCOL_VERSION: u32 = 1;

/// The default TCP port a Prism server listens on.
pub const DEFAULT_PORT: u16 = 4444;

/// The maximum frame payload size (64 MiB). A larger declared frame causes the
/// server to drop the connection.
pub const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

/// The default idle timeout, in seconds, before the server closes a quiet
/// connection.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u32 = 600;

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a packet through payload encode/decode.
    fn round_trip(p: &Packet) {
        let payload = p.to_payload().unwrap();
        let back = Packet::from_payload(&payload).unwrap();
        assert_eq!(p, &back, "payload round-trip mismatch");

        // And through a full wire frame.
        let framed = p.to_frame().unwrap();
        let (inner, consumed) = frame::parse(&framed).unwrap().expect("a complete frame");
        assert_eq!(consumed, framed.len());
        assert_eq!(Packet::from_payload(inner).unwrap(), *p);
    }

    fn sample_error() -> ErrorInfo {
        ErrorInfo {
            error_code: 0x0500,
            message: "duplicate key".into(),
            sqlstate: *b"23505",
            detail: "key (id)=(1) already exists".into(),
            position: 0,
        }
    }

    #[test]
    fn round_trips_every_control_message() {
        let messages = [
            Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                client_name: "prism-cli".into(),
                client_version: "0.1.0".into(),
                features: 0,
            },
            Message::HelloAck {
                status: 0,
                server_version: "prism 0.1.0".into(),
                features: 0,
                session_id: 0x0123_4567_89ab_cdef_0011_2233_4455_6677,
                error: None,
            },
            Message::Auth {
                mechanism: AuthMechanism::Password,
                username: "admin".into(),
                password: "hunter2".into(),
            },
            Message::Auth {
                mechanism: AuthMechanism::Mtls,
                username: "svc-billing".into(),
                password: String::new(),
            },
            Message::AuthAck {
                status: 0,
                user_oid: 42,
                error: None,
            },
            Message::Begin {
                mode: TxnMode::ReadWrite,
            },
            Message::Begin {
                mode: TxnMode::ReadOnly,
            },
            Message::Commit {
                idempotency_key: 0xdead_beef,
            },
            Message::Abort,
            Message::TxnAck {
                status: 0,
                txn_id: 1234,
                commit_lsn: 5678,
                error: None,
            },
            Message::Cancel {
                target_request_id: 99,
            },
            Message::Notice {
                severity: NoticeSeverity::Warning,
                code: 0x0001,
                message: "idle timeout approaching".into(),
            },
            Message::Ping,
            Message::Pong,
        ];
        for (i, m) in messages.into_iter().enumerate() {
            round_trip(&Packet::new(i as u32, m));
        }
    }

    #[test]
    fn error_trailer_is_present_only_when_status_nonzero() {
        // status != 0 → the trailer round-trips with the error.
        let failing = Packet::new(
            7,
            Message::TxnAck {
                status: 2,
                txn_id: 0,
                commit_lsn: 0,
                error: Some(sample_error()),
            },
        );
        round_trip(&failing);
        let payload = failing.to_payload().unwrap();
        match Packet::from_payload(&payload).unwrap().message {
            Message::TxnAck { error, .. } => assert_eq!(error, Some(sample_error())),
            other => panic!("expected TxnAck, got {other:?}"),
        }

        // A non-zero status with no explicit error still decodes to a (default)
        // trailer rather than losing the bytes.
        let defaulted = Packet::new(
            7,
            Message::AuthAck {
                status: 1,
                user_oid: 0,
                error: None,
            },
        );
        let payload = defaulted.to_payload().unwrap();
        match Packet::from_payload(&payload).unwrap().message {
            Message::AuthAck { error, .. } => assert_eq!(error, Some(ErrorInfo::default())),
            other => panic!("expected AuthAck, got {other:?}"),
        }
    }

    #[test]
    fn header_carries_type_and_request_id() {
        let p = Packet::new(0xABCD, Message::Ping);
        let payload = p.to_payload().unwrap();
        assert_eq!(payload[0], MessageType::Ping as u8);
        assert_eq!(&payload[1..4], &[0, 0, 0], "reserved bytes are zero");
        assert_eq!(&payload[4..8], &0xABCDu32.to_le_bytes());
        assert_eq!(&payload[8..12], &[0, 0, 0, 0], "reserved bytes are zero");
    }

    #[test]
    fn unknown_message_type_is_rejected() {
        let mut payload = Packet::new(1, Message::Ping).to_payload().unwrap();
        payload[0] = 0xEE; // not a known type
        assert_eq!(
            Packet::from_payload(&payload),
            Err(ProtocolError::UnknownMessageType(0xEE))
        );
    }

    #[test]
    fn truncated_payload_is_rejected() {
        let payload = Packet::new(
            1,
            Message::Hello {
                protocol_version: 1,
                client_name: "x".into(),
                client_version: "y".into(),
                features: 0,
            },
        )
        .to_payload()
        .unwrap();
        // Drop the last byte: decoding must fail rather than panic or silently
        // succeed.
        let truncated = &payload[..payload.len() - 1];
        assert!(matches!(
            Packet::from_payload(truncated),
            Err(ProtocolError::Truncated { .. })
        ));
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut payload = Packet::new(1, Message::Ping).to_payload().unwrap();
        payload.push(0xFF); // junk after a body-less message
        assert!(matches!(
            Packet::from_payload(&payload),
            Err(ProtocolError::TrailingBytes { count: 1 })
        ));
    }

    #[test]
    fn bad_enum_discriminant_is_rejected() {
        let mut payload = Packet::new(
            1,
            Message::Begin {
                mode: TxnMode::ReadWrite,
            },
        )
        .to_payload()
        .unwrap();
        *payload.last_mut().unwrap() = 9; // invalid TxnMode
        assert!(matches!(
            Packet::from_payload(&payload),
            Err(ProtocolError::BadEnum {
                field: "begin.mode",
                ..
            })
        ));
    }

    #[test]
    fn frame_parse_needs_a_full_frame() {
        let framed = Packet::new(1, Message::Ping).to_frame().unwrap();
        // Length prefix not yet complete.
        assert_eq!(frame::parse(&framed[..2]).unwrap(), None);
        // Prefix present, body not yet fully arrived.
        assert_eq!(frame::parse(&framed[..framed.len() - 1]).unwrap(), None);
        // Whole frame: parses.
        assert!(frame::parse(&framed).unwrap().is_some());
    }

    #[test]
    fn frame_parse_handles_back_to_back_frames() {
        // Two frames concatenated in one buffer (as they arrive from a socket).
        let a = Packet::new(1, Message::Ping).to_frame().unwrap();
        let b = Packet::new(2, Message::Pong).to_frame().unwrap();
        let mut buf = a.clone();
        buf.extend_from_slice(&b);

        let (p1, used1) = frame::parse(&buf).unwrap().unwrap();
        assert_eq!(used1, a.len());
        assert_eq!(Packet::from_payload(p1).unwrap().request_id, 1);

        let (p2, used2) = frame::parse(&buf[used1..]).unwrap().unwrap();
        assert_eq!(used2, b.len());
        assert_eq!(Packet::from_payload(p2).unwrap().request_id, 2);
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut buf = ((MAX_FRAME_SIZE + 1) as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(&[0u8; 8]);
        assert_eq!(
            frame::parse(&buf),
            Err(ProtocolError::FrameTooLarge {
                len: MAX_FRAME_SIZE + 1
            })
        );
    }
}
