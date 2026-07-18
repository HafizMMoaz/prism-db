//! `prism-protocol` - wire protocol types and serialization.
//!
//! Message enums and their stable little-endian binary encoding, plus the
//! length-prefixed framing - pure data definitions with no socket I/O, so both
//! the server and every client depend on it without pulling in the engine. See
//! `docs/specs/wire-protocol.md` and `docs/adr/0008-binary-wire-protocol.md`.
//!
//! The on-wire shape is `[length: u32][payload]`, where the payload is a 12-byte
//! common header (message type + `request_id`) followed by a message-specific
//! body. [`Packet`] is the header + body; [`frame`] adds and strips the length
//! prefix.
//!
//! **Coverage:** the full v1 message set - the session and transaction control
//! plane (handshake, authentication, `Begin`/`Commit`/`Abort`, cancellation,
//! notices, keep-alive) and the query data plane (`SqlExecute`/`SqlResult`,
//! `DocOp`/`DocResult`, `KvOp`/`KvResult`), plus framing, the little-endian
//! codec, and the error trailer. SQL parameters and result cells use [`Value`]
//! (the spec's `TaggedValue`); documents, keys, and values are opaque byte
//! strings decoded by the engines. Deferred: nested Array/Object as standalone
//! wire values (they ride inside opaque document bytes), and `Decimal`.

pub mod codec;
pub mod data;
pub mod error;
pub mod frame;
pub mod message;

pub use data::{
    ColumnDesc, DocCommand, DocQuery, DocUpdate, DocUpdateOp, KvCommand, KvResultBody, Row, Value,
};
pub use error::{ProtocolError, Result};
pub use message::{
    AuthMechanism, ErrorInfo, Message, MessageType, NoticeSeverity, Packet, TxnMode,
};

/// The protocol version carried in `Hello` (`docs/specs/wire-protocol.md`).
pub const PROTOCOL_VERSION: u32 = 1;

/// `Hello` feature bit: the client carries a connect-time database name in its
/// `Hello` body, binding the session to that database during the handshake
/// instead of issuing a separate `USE <db>`. When the bit is clear the
/// `database` field is absent from the wire, so a feature-unaware peer round
/// trips unchanged. The server echoes the bits it honored in `HelloAck`.
pub const FEATURE_CONNECT_DB: u32 = 1 << 0;

/// The feature bits this build understands and will negotiate in `HelloAck`.
pub const SERVER_FEATURES: u32 = FEATURE_CONNECT_DB;

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
    fn round_trips_every_data_plane_message() {
        let messages = [
            Message::SqlExecute {
                sql: "SELECT * FROM accounts WHERE id = ?".into(),
                params: vec![
                    Value::Int64(7),
                    Value::Str("alice".into()),
                    Value::Null,
                    Value::Bool(true),
                    Value::Double(2.5),
                    Value::Int32(-3),
                    Value::Timestamp(1_700_000_000_000_000),
                    Value::ObjectId([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
                    Value::Binary {
                        subtype: 0,
                        bytes: vec![0xDE, 0xAD, 0xBE, 0xEF],
                    },
                ],
                options: 1,
            },
            Message::SqlResult {
                status: 0,
                affected_rows: 0,
                columns: vec![
                    ColumnDesc {
                        name: "id".into(),
                        type_tag: 0x03, // Int64
                        nullable: false,
                    },
                    ColumnDesc {
                        name: "name".into(),
                        type_tag: 0x05, // String
                        nullable: true,
                    },
                ],
                rows: vec![
                    vec![Some(Value::Int64(1)), Some(Value::Str("alice".into()))],
                    vec![Some(Value::Int64(2)), None], // NULL name
                ],
                more_frames: false,
                error: None,
            },
            Message::DocOp {
                collection: "users".into(),
                command: DocCommand::InsertMany(vec![vec![1, 2, 3], vec![4, 5]]),
            },
            Message::DocOp {
                collection: "users".into(),
                command: DocCommand::UpdateOne {
                    query: vec![0x10],
                    update: vec![0x20, 0x21],
                    options: vec![],
                },
            },
            Message::DocResult {
                status: 0,
                affected: 2,
                inserted_ids: vec![[7u8; 12], [9u8; 12]],
                docs: vec![vec![0xAA, 0xBB]],
                more_frames: true,
                error: None,
            },
            Message::KvOp {
                namespace: "sessions".into(),
                command: KvCommand::Put {
                    key: b"k1".to_vec(),
                    value: b"v1".to_vec(),
                },
            },
            Message::KvOp {
                namespace: "sessions".into(),
                command: KvCommand::Range {
                    start: b"a".to_vec(),
                    end: b"z".to_vec(),
                    max_results: 100,
                },
            },
            Message::KvResult {
                status: 0,
                body: KvResultBody::Get {
                    value: Some(b"hello".to_vec()),
                },
                error: None,
            },
            Message::KvResult {
                status: 0,
                body: KvResultBody::Scan {
                    entries: vec![
                        (b"k1".to_vec(), b"v1".to_vec()),
                        (b"k2".to_vec(), b"v2".to_vec()),
                    ],
                    more_frames: false,
                },
                error: None,
            },
            Message::KvResult {
                status: 0,
                body: KvResultBody::Get { value: None },
                error: None,
            },
        ];
        for (i, m) in messages.into_iter().enumerate() {
            round_trip(&Packet::new(i as u32, m));
        }
    }

    #[test]
    fn sql_result_error_trailer_round_trips() {
        let failing = Packet::new(
            3,
            Message::SqlResult {
                status: 5,
                affected_rows: 0,
                columns: vec![],
                rows: vec![],
                more_frames: false,
                error: Some(sample_error()),
            },
        );
        round_trip(&failing);
        match Packet::from_payload(&failing.to_payload().unwrap())
            .unwrap()
            .message
        {
            Message::SqlResult { error, .. } => assert_eq!(error, Some(sample_error())),
            other => panic!("expected SqlResult, got {other:?}"),
        }
    }

    #[test]
    fn unknown_value_tag_in_param_is_rejected() {
        // A SqlExecute with one Int64 param; corrupt its type tag to a reserved
        // one (0x0B Decimal) and confirm decode rejects it rather than panicking.
        let p = Packet::new(
            1,
            Message::SqlExecute {
                sql: "x".into(),
                params: vec![Value::Int64(1)],
                options: 0,
            },
        );
        let mut payload = p.to_payload().unwrap();
        // Layout: 12-byte header, u32 sql_len(=1), 1 sql byte, u16 param_count(=1),
        // then the tagged value: [type_tag][8 bytes]. The tag sits right after
        // the param count.
        let tag_pos = HEADER_SIZE + 4 + 1 + 2;
        payload[tag_pos] = 0x0B; // Decimal (reserved, not wire-encodable)
        assert_eq!(
            Packet::from_payload(&payload),
            Err(ProtocolError::UnknownValueType(0x0B))
        );
    }

    #[test]
    fn unknown_doc_op_type_is_rejected() {
        let mut payload = Packet::new(
            1,
            Message::DocOp {
                collection: "c".into(),
                command: DocCommand::InsertOne(vec![1]),
            },
        )
        .to_payload()
        .unwrap();
        payload[HEADER_SIZE] = 99; // op_type byte
        assert!(matches!(
            Packet::from_payload(&payload),
            Err(ProtocolError::UnknownOpType {
                family: "document",
                value: 99
            })
        ));
    }

    #[test]
    fn value_too_large_for_its_length_prefix_is_reported() {
        // A KV key is carried with a u16 length prefix; one larger than 65535
        // bytes must fail to encode rather than silently truncate.
        let oversize = Packet::new(
            1,
            Message::KvOp {
                namespace: "n".into(),
                command: KvCommand::Get {
                    key: vec![0u8; 70_000],
                },
            },
        );
        assert_eq!(
            oversize.to_payload(),
            Err(ProtocolError::ValueTooLarge { field: "kv.key" })
        );
    }

    const HEADER_SIZE: usize = 12;

    #[test]
    fn round_trips_every_control_message() {
        let messages = [
            Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                client_name: "prism-cli".into(),
                client_version: "0.1.0".into(),
                features: 0,
                database: String::new(),
            },
            // A connect-time database carried under its feature bit.
            Message::hello(PROTOCOL_VERSION, "prism-cli", "0.1.0", "analytics"),
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
                database: String::new(),
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

    #[test]
    fn doc_query_round_trips_through_bytes() {
        // A nested filter exercising every variant family.
        let q = DocQuery::And(vec![
            DocQuery::Or(vec![
                DocQuery::Eq("name".into(), Value::Str("alice".into())),
                DocQuery::Gt("age".into(), Value::Int64(30)),
            ]),
            DocQuery::In(
                "tier".into(),
                vec![Value::Int32(1), Value::Int32(2), Value::Null],
            ),
            DocQuery::Nin("flag".into(), vec![Value::Bool(true)]),
            DocQuery::Not(Box::new(DocQuery::Lte("score".into(), Value::Double(1.5)))),
            DocQuery::Exists("email".into(), true),
            DocQuery::All,
        ]);
        let bytes = q.to_bytes().unwrap();
        assert_eq!(DocQuery::from_bytes(&bytes).unwrap(), q);
    }

    #[test]
    fn doc_update_round_trips_through_bytes() {
        let u = DocUpdate {
            ops: vec![
                DocUpdateOp::Set("name".into(), Value::Str("alice".into())),
                DocUpdateOp::Unset("temp".into()),
                DocUpdateOp::Inc("visits".into(), -3),
            ],
        };
        let bytes = u.to_bytes().unwrap();
        assert_eq!(DocUpdate::from_bytes(&bytes).unwrap(), u);
        // Unknown op tag is a BadEnum.
        assert!(matches!(
            DocUpdate::from_bytes(&[1, 0, 0, 0, 0xEE]),
            Err(ProtocolError::BadEnum {
                field: "update.tag",
                ..
            })
        ));
    }

    #[test]
    fn doc_query_rejects_unknown_tag_and_trailing_bytes() {
        // An unknown discriminant is a BadEnum.
        assert!(matches!(
            DocQuery::from_bytes(&[0xEE]),
            Err(ProtocolError::BadEnum {
                field: "query.tag",
                ..
            })
        ));
        // Extra bytes after a complete query are rejected.
        let mut bytes = DocQuery::All.to_bytes().unwrap();
        bytes.push(0x00);
        assert!(matches!(
            DocQuery::from_bytes(&bytes),
            Err(ProtocolError::TrailingBytes { .. })
        ));
    }
}
