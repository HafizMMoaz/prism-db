//! The control-plane messages and the packet (header + body) codec.
//!
//! This slice covers the session and transaction control plane from
//! `docs/specs/wire-protocol.md`: the handshake (`Hello`/`Auth` and their acks),
//! transaction control (`Begin`/`Commit`/`Abort`/`TxnAck`), cancellation,
//! notices, and keep-alive (`Ping`/`Pong`). The query data plane
//! (`SqlExecute`/`DocOp`/`KvOp` and their results) is a follow-up increment.

use crate::codec::{Reader, Writer};
use crate::error::{ProtocolError, Result};
use crate::frame;

/// The 12-byte common payload header: `msg_type | reserved[3] | request_id |
/// reserved[4]`.
const HEADER_SIZE: usize = 12;

/// The message-type discriminant (the first header byte). Codes are fixed by the
/// wire spec; gaps are intentional (data-plane codes land in their own ranges).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum MessageType {
    /// Client → server handshake.
    Hello = 0x01,
    /// Server → client handshake acknowledgement.
    HelloAck = 0x02,
    /// Client → server authentication.
    Auth = 0x03,
    /// Server → client authentication acknowledgement.
    AuthAck = 0x04,
    /// Client → server: begin a transaction.
    Begin = 0x10,
    /// Client → server: commit the current transaction.
    Commit = 0x11,
    /// Client → server: abort the current transaction.
    Abort = 0x12,
    /// Server → client: transaction control acknowledgement.
    TxnAck = 0x13,
    /// Client → server: cancel an in-flight request.
    Cancel = 0x50,
    /// Server → client: an unsolicited connection-level event.
    Notice = 0x60,
    /// Client → server: keep-alive.
    Ping = 0x70,
    /// Server → client: keep-alive reply.
    Pong = 0x71,
}

impl TryFrom<u8> for MessageType {
    type Error = ProtocolError;
    fn try_from(v: u8) -> Result<Self> {
        Ok(match v {
            0x01 => MessageType::Hello,
            0x02 => MessageType::HelloAck,
            0x03 => MessageType::Auth,
            0x04 => MessageType::AuthAck,
            0x10 => MessageType::Begin,
            0x11 => MessageType::Commit,
            0x12 => MessageType::Abort,
            0x13 => MessageType::TxnAck,
            0x50 => MessageType::Cancel,
            0x60 => MessageType::Notice,
            0x70 => MessageType::Ping,
            0x71 => MessageType::Pong,
            other => return Err(ProtocolError::UnknownMessageType(other)),
        })
    }
}

/// The authentication mechanism in an `Auth` message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthMechanism {
    /// Username + password.
    Password,
    /// Mutual TLS — the certificate is presented at the TLS layer; only the
    /// username travels in the message body.
    Mtls,
}

impl AuthMechanism {
    fn code(self) -> u8 {
        match self {
            AuthMechanism::Password => 1,
            AuthMechanism::Mtls => 2,
        }
    }
    fn from_code(v: u8) -> Result<Self> {
        match v {
            1 => Ok(AuthMechanism::Password),
            2 => Ok(AuthMechanism::Mtls),
            other => Err(ProtocolError::BadEnum {
                field: "auth.mechanism",
                value: other,
            }),
        }
    }
}

/// The mode of a transaction in a `Begin` message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TxnMode {
    /// Read-write.
    ReadWrite,
    /// Read-only.
    ReadOnly,
}

impl TxnMode {
    fn code(self) -> u8 {
        match self {
            TxnMode::ReadWrite => 0,
            TxnMode::ReadOnly => 1,
        }
    }
    fn from_code(v: u8) -> Result<Self> {
        match v {
            0 => Ok(TxnMode::ReadWrite),
            1 => Ok(TxnMode::ReadOnly),
            other => Err(ProtocolError::BadEnum {
                field: "begin.mode",
                value: other,
            }),
        }
    }
}

/// The severity of a `Notice`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NoticeSeverity {
    /// Informational.
    Info,
    /// Warning.
    Warning,
    /// Error.
    Error,
}

impl NoticeSeverity {
    fn code(self) -> u8 {
        match self {
            NoticeSeverity::Info => 0,
            NoticeSeverity::Warning => 1,
            NoticeSeverity::Error => 2,
        }
    }
    fn from_code(v: u8) -> Result<Self> {
        match v {
            0 => Ok(NoticeSeverity::Info),
            1 => Ok(NoticeSeverity::Warning),
            2 => Ok(NoticeSeverity::Error),
            other => Err(ProtocolError::BadEnum {
                field: "notice.severity",
                value: other,
            }),
        }
    }
}

/// The error trailer appended to any server response whose `status` is non-zero
/// (`docs/specs/wire-protocol.md`, "Errors").
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ErrorInfo {
    /// A code from the spec's error-code ranges.
    pub error_code: u32,
    /// A human-readable message.
    pub message: String,
    /// A 5-byte ASCII SQLSTATE (e.g. `b"23505"`).
    pub sqlstate: [u8; 5],
    /// Optional extra detail (may be empty).
    pub detail: String,
    /// Character position in the source SQL, or 0 if not applicable.
    pub position: u32,
}

impl Default for ErrorInfo {
    fn default() -> Self {
        Self {
            error_code: 0,
            message: String::new(),
            sqlstate: *b"00000",
            detail: String::new(),
            position: 0,
        }
    }
}

impl ErrorInfo {
    fn encode(&self, w: &mut Writer) -> Result<()> {
        w.put_u32(self.error_code);
        w.put_str_u16("error.message", &self.message)?;
        w.put_raw(&self.sqlstate);
        w.put_str_u16("error.detail", &self.detail)?;
        w.put_u32(self.position);
        Ok(())
    }
    fn decode(r: &mut Reader) -> Result<Self> {
        Ok(Self {
            error_code: r.get_u32("error.error_code")?,
            message: r.get_str_u16("error.message")?,
            sqlstate: r.get_array::<5>("error.sqlstate")?,
            detail: r.get_str_u16("error.detail")?,
            position: r.get_u32("error.position")?,
        })
    }
}

/// Write the error trailer iff `status` is non-zero (an absent `error` defaults
/// to a blank trailer, so a non-zero status always has a well-formed trailer).
fn put_trailer(w: &mut Writer, status: u8, error: &Option<ErrorInfo>) -> Result<()> {
    if status != 0 {
        error.clone().unwrap_or_default().encode(w)?;
    }
    Ok(())
}

/// Read the error trailer iff `status` is non-zero.
fn get_trailer(r: &mut Reader, status: u8) -> Result<Option<ErrorInfo>> {
    if status != 0 {
        Ok(Some(ErrorInfo::decode(r)?))
    } else {
        Ok(None)
    }
}

/// A decoded protocol message body (the control plane).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Message {
    /// `Hello` (0x01).
    Hello {
        /// The protocol version the client speaks (currently [`crate::PROTOCOL_VERSION`]).
        protocol_version: u32,
        /// The client implementation's name.
        client_name: String,
        /// The client implementation's version.
        client_version: String,
        /// Reserved feature bitmask (send 0).
        features: u32,
    },
    /// `HelloAck` (0x02).
    HelloAck {
        /// 0 = OK.
        status: u8,
        /// The server's version string.
        server_version: String,
        /// Negotiated feature bitmask.
        features: u32,
        /// A random session id, logged for traceability.
        session_id: u128,
        /// Present iff `status != 0`.
        error: Option<ErrorInfo>,
    },
    /// `Auth` (0x03).
    Auth {
        /// The authentication mechanism.
        mechanism: AuthMechanism,
        /// The username.
        username: String,
        /// The password (empty for [`AuthMechanism::Mtls`]).
        password: String,
    },
    /// `AuthAck` (0x04).
    AuthAck {
        /// 0 = OK, 1 = bad credentials, 2 = no such user.
        status: u8,
        /// The authenticated user's OID (0 on failure).
        user_oid: u64,
        /// Present iff `status != 0`.
        error: Option<ErrorInfo>,
    },
    /// `Begin` (0x10).
    Begin {
        /// The transaction mode.
        mode: TxnMode,
    },
    /// `Commit` (0x11).
    Commit {
        /// An idempotency key (0 = none).
        idempotency_key: u128,
    },
    /// `Abort` (0x12).
    Abort,
    /// `TxnAck` (0x13).
    TxnAck {
        /// 0 = OK.
        status: u8,
        /// The assigned `TxnId` on begin; the current one otherwise.
        txn_id: u64,
        /// The commit LSN on commit; 0 otherwise.
        commit_lsn: u64,
        /// Present iff `status != 0`.
        error: Option<ErrorInfo>,
    },
    /// `Cancel` (0x50).
    Cancel {
        /// The `request_id` of the in-flight request to abort.
        target_request_id: u32,
    },
    /// `Notice` (0x60).
    Notice {
        /// The severity.
        severity: NoticeSeverity,
        /// A notice code.
        code: u32,
        /// A human-readable message.
        message: String,
    },
    /// `Ping` (0x70).
    Ping,
    /// `Pong` (0x71).
    Pong,
}

impl Message {
    /// The message-type discriminant for this message.
    pub fn message_type(&self) -> MessageType {
        match self {
            Message::Hello { .. } => MessageType::Hello,
            Message::HelloAck { .. } => MessageType::HelloAck,
            Message::Auth { .. } => MessageType::Auth,
            Message::AuthAck { .. } => MessageType::AuthAck,
            Message::Begin { .. } => MessageType::Begin,
            Message::Commit { .. } => MessageType::Commit,
            Message::Abort => MessageType::Abort,
            Message::TxnAck { .. } => MessageType::TxnAck,
            Message::Cancel { .. } => MessageType::Cancel,
            Message::Notice { .. } => MessageType::Notice,
            Message::Ping => MessageType::Ping,
            Message::Pong => MessageType::Pong,
        }
    }

    fn encode_body(&self, w: &mut Writer) -> Result<()> {
        match self {
            Message::Hello {
                protocol_version,
                client_name,
                client_version,
                features,
            } => {
                w.put_u32(*protocol_version);
                w.put_str_u16("hello.client_name", client_name)?;
                w.put_str_u16("hello.client_version", client_version)?;
                w.put_u32(*features);
            }
            Message::HelloAck {
                status,
                server_version,
                features,
                session_id,
                error,
            } => {
                w.put_u8(*status);
                w.put_str_u16("hello_ack.server_version", server_version)?;
                w.put_u32(*features);
                w.put_u128(*session_id);
                put_trailer(w, *status, error)?;
            }
            Message::Auth {
                mechanism,
                username,
                password,
            } => {
                w.put_u8(mechanism.code());
                w.put_str_u16("auth.username", username)?;
                if *mechanism == AuthMechanism::Password {
                    w.put_str_u16("auth.password", password)?;
                }
            }
            Message::AuthAck {
                status,
                user_oid,
                error,
            } => {
                w.put_u8(*status);
                w.put_u64(*user_oid);
                put_trailer(w, *status, error)?;
            }
            Message::Begin { mode } => w.put_u8(mode.code()),
            Message::Commit { idempotency_key } => w.put_u128(*idempotency_key),
            Message::Abort => {}
            Message::TxnAck {
                status,
                txn_id,
                commit_lsn,
                error,
            } => {
                w.put_u8(*status);
                w.put_u64(*txn_id);
                w.put_u64(*commit_lsn);
                put_trailer(w, *status, error)?;
            }
            Message::Cancel { target_request_id } => w.put_u32(*target_request_id),
            Message::Notice {
                severity,
                code,
                message,
            } => {
                w.put_u8(severity.code());
                w.put_u32(*code);
                w.put_str_u16("notice.message", message)?;
            }
            Message::Ping | Message::Pong => {}
        }
        Ok(())
    }

    fn decode_body(ty: MessageType, r: &mut Reader) -> Result<Message> {
        Ok(match ty {
            MessageType::Hello => Message::Hello {
                protocol_version: r.get_u32("hello.protocol_version")?,
                client_name: r.get_str_u16("hello.client_name")?,
                client_version: r.get_str_u16("hello.client_version")?,
                features: r.get_u32("hello.features")?,
            },
            MessageType::HelloAck => {
                let status = r.get_u8("hello_ack.status")?;
                let server_version = r.get_str_u16("hello_ack.server_version")?;
                let features = r.get_u32("hello_ack.features")?;
                let session_id = r.get_u128("hello_ack.session_id")?;
                Message::HelloAck {
                    status,
                    server_version,
                    features,
                    session_id,
                    error: get_trailer(r, status)?,
                }
            }
            MessageType::Auth => {
                let mechanism = AuthMechanism::from_code(r.get_u8("auth.mechanism")?)?;
                let username = r.get_str_u16("auth.username")?;
                let password = if mechanism == AuthMechanism::Password {
                    r.get_str_u16("auth.password")?
                } else {
                    String::new()
                };
                Message::Auth {
                    mechanism,
                    username,
                    password,
                }
            }
            MessageType::AuthAck => {
                let status = r.get_u8("auth_ack.status")?;
                let user_oid = r.get_u64("auth_ack.user_oid")?;
                Message::AuthAck {
                    status,
                    user_oid,
                    error: get_trailer(r, status)?,
                }
            }
            MessageType::Begin => Message::Begin {
                mode: TxnMode::from_code(r.get_u8("begin.mode")?)?,
            },
            MessageType::Commit => Message::Commit {
                idempotency_key: r.get_u128("commit.idempotency_key")?,
            },
            MessageType::Abort => Message::Abort,
            MessageType::TxnAck => {
                let status = r.get_u8("txn_ack.status")?;
                let txn_id = r.get_u64("txn_ack.txn_id")?;
                let commit_lsn = r.get_u64("txn_ack.commit_lsn")?;
                Message::TxnAck {
                    status,
                    txn_id,
                    commit_lsn,
                    error: get_trailer(r, status)?,
                }
            }
            MessageType::Cancel => Message::Cancel {
                target_request_id: r.get_u32("cancel.target_request_id")?,
            },
            MessageType::Notice => Message::Notice {
                severity: NoticeSeverity::from_code(r.get_u8("notice.severity")?)?,
                code: r.get_u32("notice.code")?,
                message: r.get_str_u16("notice.message")?,
            },
            MessageType::Ping => Message::Ping,
            MessageType::Pong => Message::Pong,
        })
    }
}

/// A full protocol packet: a `request_id` plus a [`Message`]. This is the unit
/// the spec calls a "payload" — the 12-byte common header followed by the
/// message-specific body. Frame it with [`frame::encode`] to put it on the wire.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Packet {
    /// Client-assigned on client→server frames; echoed on the reply. 0 for
    /// server-initiated frames such as `Notice`.
    pub request_id: u32,
    /// The message body.
    pub message: Message,
}

impl Packet {
    /// A new packet.
    pub fn new(request_id: u32, message: Message) -> Self {
        Self {
            request_id,
            message,
        }
    }

    /// Encode the payload: the 12-byte common header followed by the body.
    pub fn to_payload(&self) -> Result<Vec<u8>> {
        let mut w = Writer::with_capacity(HEADER_SIZE + 16);
        w.put_u8(self.message.message_type() as u8);
        w.put_zeros(3); // reserved
        w.put_u32(self.request_id);
        w.put_zeros(4); // reserved
        self.message.encode_body(&mut w)?;
        Ok(w.into_vec())
    }

    /// Decode a payload (header + body) into a packet.
    pub fn from_payload(payload: &[u8]) -> Result<Self> {
        let mut r = Reader::new(payload);
        let ty = MessageType::try_from(r.get_u8("header.msg_type")?)?;
        r.skip(3, "header.reserved")?;
        let request_id = r.get_u32("header.request_id")?;
        r.skip(4, "header.reserved")?;
        let message = Message::decode_body(ty, &mut r)?;
        r.expect_end()?;
        Ok(Packet {
            request_id,
            message,
        })
    }

    /// Encode straight to a length-prefixed wire frame.
    pub fn to_frame(&self) -> Result<Vec<u8>> {
        Ok(frame::encode(&self.to_payload()?))
    }
}
