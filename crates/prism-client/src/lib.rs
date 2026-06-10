//! `prism-client` — an async client for the Prism wire protocol.
//!
//! A thin, typed wrapper over `prism-protocol`: connect over TCP, complete the
//! `Hello`/`Auth` handshake, then issue SQL / KV / document requests and read
//! typed responses. One request is in flight at a time per [`Client`] (responses
//! are matched by `request_id`; multiplexing is a future addition).
//!
//! This crate is a leaf — it depends only on the protocol types and tokio, never
//! on the server or engines (`docs/architecture/module-layout.md`). Documents
//! are exchanged as opaque tagged-binary bytes; build and parse them with
//! `prism-doc` on the application side.

use std::io;
use std::sync::Arc;

use prism_protocol::{
    AuthMechanism, ColumnDesc, DocCommand, ErrorInfo, KvCommand, KvResultBody, Message,
    PROTOCOL_VERSION, Packet, ProtocolError, TxnMode, Value, frame,
};
use rustls::RootCertStore;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio_rustls::TlsConnector;

/// A byte transport the client speaks the protocol over (plaintext TCP or TLS).
trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Transport for T {}

/// Build a rustls client config trusting `roots` (ring crypto backend).
pub fn tls_client_config(roots: RootCertStore) -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, ClientError>;

/// An error talking to a Prism server.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// A transport error.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// A frame or message could not be decoded.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    /// The server returned an error response.
    #[error("server error {code:#06x} [{sqlstate}]: {message}")]
    Server {
        /// The wire error code.
        code: u32,
        /// The human-readable message.
        message: String,
        /// The 5-char SQLSTATE.
        sqlstate: String,
    },
    /// Authentication was rejected.
    #[error("authentication failed")]
    AuthFailed,
    /// The server sent a message of an unexpected type.
    #[error("unexpected response: {0}")]
    Unexpected(String),
}

/// The result of a SQL statement.
#[derive(Clone, Debug, Default)]
pub struct QueryResult {
    /// Output columns (empty for non-SELECT).
    pub columns: Vec<ColumnDesc>,
    /// Result rows, each cell `None` for SQL NULL.
    pub rows: Vec<Vec<Option<Value>>>,
    /// Rows affected by INSERT/UPDATE/DELETE (0 for SELECT).
    pub affected: u64,
}

/// The result of a document operation.
#[derive(Clone, Debug, Default)]
pub struct DocReply {
    /// Documents matched/modified/deleted, or inserted.
    pub affected: u64,
    /// `_id`s assigned by inserts.
    pub inserted_ids: Vec<[u8; 12]>,
    /// Returned documents (opaque tagged-binary bytes).
    pub docs: Vec<Vec<u8>>,
}

/// A connected, authenticated (after [`Client::authenticate`]) client over
/// either a plaintext or a TLS transport.
pub struct Client {
    stream: Box<dyn Transport>,
    next_id: u32,
    buf: Vec<u8>,
}

impl Client {
    /// Connect to `addr` over plaintext TCP and complete the `Hello` handshake.
    pub async fn connect(addr: impl ToSocketAddrs) -> Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        Self::with_transport(Box::new(stream)).await
    }

    /// Connect to `addr` over TLS (verifying the server's certificate against
    /// `config`, presented for `server_name`) and complete the handshake.
    pub async fn connect_tls(
        addr: impl ToSocketAddrs,
        server_name: &str,
        config: Arc<rustls::ClientConfig>,
    ) -> Result<Self> {
        let tcp = TcpStream::connect(addr).await?;
        let name = ServerName::try_from(server_name.to_string())
            .map_err(|e| ClientError::Unexpected(format!("invalid server name: {e}")))?;
        let tls = TlsConnector::from(config).connect(name, tcp).await?;
        Self::with_transport(Box::new(tls)).await
    }

    /// Complete the `Hello` handshake over an established transport.
    async fn with_transport(stream: Box<dyn Transport>) -> Result<Self> {
        let mut client = Self {
            stream,
            next_id: 1,
            buf: Vec::with_capacity(8192),
        };
        match client
            .request(Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                client_name: "prism-client".into(),
                client_version: env!("CARGO_PKG_VERSION").into(),
                features: 0,
            })
            .await?
        {
            Message::HelloAck { status: 0, .. } => Ok(client),
            Message::HelloAck { error, .. } => Err(server_error(error)),
            other => Err(unexpected(&other)),
        }
    }

    /// Connect (plaintext) and authenticate in one step.
    pub async fn connect_authenticated(
        addr: impl ToSocketAddrs,
        username: &str,
        password: &str,
    ) -> Result<Self> {
        let mut client = Self::connect(addr).await?;
        client.authenticate(username, password).await?;
        Ok(client)
    }

    /// Authenticate with a username and password; returns the user OID.
    pub async fn authenticate(&mut self, username: &str, password: &str) -> Result<u64> {
        match self
            .request(Message::Auth {
                mechanism: AuthMechanism::Password,
                username: username.to_string(),
                password: password.to_string(),
            })
            .await?
        {
            Message::AuthAck {
                status: 0,
                user_oid,
                ..
            } => Ok(user_oid),
            Message::AuthAck { .. } => Err(ClientError::AuthFailed),
            other => Err(unexpected(&other)),
        }
    }

    /// Round-trip a keep-alive.
    pub async fn ping(&mut self) -> Result<()> {
        match self.request(Message::Ping).await? {
            Message::Pong => Ok(()),
            other => Err(unexpected(&other)),
        }
    }

    /// Begin an explicit transaction; returns its id.
    pub async fn begin(&mut self, read_only: bool) -> Result<u64> {
        let mode = if read_only {
            TxnMode::ReadOnly
        } else {
            TxnMode::ReadWrite
        };
        self.txn_ack(Message::Begin { mode }).await
    }

    /// Commit the current transaction.
    pub async fn commit(&mut self) -> Result<()> {
        self.txn_ack(Message::Commit { idempotency_key: 0 })
            .await
            .map(|_| ())
    }

    /// Abort the current transaction.
    pub async fn abort(&mut self) -> Result<()> {
        self.txn_ack(Message::Abort).await.map(|_| ())
    }

    async fn txn_ack(&mut self, message: Message) -> Result<u64> {
        match self.request(message).await? {
            Message::TxnAck {
                status: 0, txn_id, ..
            } => Ok(txn_id),
            Message::TxnAck { error, .. } => Err(server_error(error)),
            other => Err(unexpected(&other)),
        }
    }

    /// Execute a SQL statement.
    pub async fn sql(&mut self, sql: &str) -> Result<QueryResult> {
        match self
            .request(Message::SqlExecute {
                sql: sql.to_string(),
                params: vec![],
                options: 1,
            })
            .await?
        {
            Message::SqlResult {
                status: 0,
                affected_rows,
                columns,
                rows,
                ..
            } => Ok(QueryResult {
                columns,
                rows,
                affected: affected_rows,
            }),
            Message::SqlResult { error, .. } => Err(server_error(error)),
            other => Err(unexpected(&other)),
        }
    }

    /// Get a KV value.
    pub async fn kv_get(&mut self, namespace: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match self
            .kv(namespace, KvCommand::Get { key: key.to_vec() })
            .await?
        {
            KvResultBody::Get { value } => Ok(value),
            other => Err(ClientError::Unexpected(format!("{other:?}"))),
        }
    }

    /// Put a KV value (upsert).
    pub async fn kv_put(&mut self, namespace: &str, key: &[u8], value: &[u8]) -> Result<()> {
        self.kv(
            namespace,
            KvCommand::Put {
                key: key.to_vec(),
                value: value.to_vec(),
            },
        )
        .await
        .map(|_| ())
    }

    /// Delete a KV key.
    pub async fn kv_delete(&mut self, namespace: &str, key: &[u8]) -> Result<()> {
        self.kv(namespace, KvCommand::Delete { key: key.to_vec() })
            .await
            .map(|_| ())
    }

    async fn kv(&mut self, namespace: &str, command: KvCommand) -> Result<KvResultBody> {
        match self
            .request(Message::KvOp {
                namespace: namespace.to_string(),
                command,
            })
            .await?
        {
            Message::KvResult {
                status: 0, body, ..
            } => Ok(body),
            Message::KvResult { error, .. } => Err(server_error(error)),
            other => Err(unexpected(&other)),
        }
    }

    /// Insert one document (tagged-binary bytes); returns the assigned `_id`s.
    pub async fn doc_insert_one(&mut self, collection: &str, doc: Vec<u8>) -> Result<DocReply> {
        self.doc(collection, DocCommand::InsertOne(doc)).await
    }

    /// Find documents matching `query` (a tagged-binary query document).
    pub async fn doc_find(&mut self, collection: &str, query: Vec<u8>) -> Result<DocReply> {
        self.doc(
            collection,
            DocCommand::Find {
                query,
                options: vec![],
            },
        )
        .await
    }

    /// Run an arbitrary document command (escape hatch for the full op set).
    pub async fn doc(&mut self, collection: &str, command: DocCommand) -> Result<DocReply> {
        match self
            .request(Message::DocOp {
                collection: collection.to_string(),
                command,
            })
            .await?
        {
            Message::DocResult {
                status: 0,
                affected,
                inserted_ids,
                docs,
                ..
            } => Ok(DocReply {
                affected,
                inserted_ids,
                docs,
            }),
            Message::DocResult { error, .. } => Err(server_error(error)),
            other => Err(unexpected(&other)),
        }
    }

    /// Send one request and read its matching response. Unsolicited server
    /// notices (`request_id` 0) are skipped.
    async fn request(&mut self, message: Message) -> Result<Message> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let frame = Packet::new(id, message).to_frame()?;
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;

        loop {
            let parsed =
                frame::parse(&self.buf)?.map(|(payload, consumed)| (payload.to_vec(), consumed));
            if let Some((payload, consumed)) = parsed {
                self.buf.drain(..consumed);
                let packet = Packet::from_payload(&payload)?;
                if packet.request_id == 0 {
                    continue; // unsolicited notice; ignore and keep reading
                }
                return Ok(packet.message);
            }
            let mut chunk = [0u8; 8192];
            let n = self.stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(ClientError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "server closed the connection",
                )));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

fn server_error(info: Option<ErrorInfo>) -> ClientError {
    match info {
        Some(e) => ClientError::Server {
            code: e.error_code,
            message: e.message,
            sqlstate: String::from_utf8_lossy(&e.sqlstate).into_owned(),
        },
        None => ClientError::Server {
            code: 0,
            message: "unspecified server error".into(),
            sqlstate: "XX000".into(),
        },
    }
}

fn unexpected(message: &Message) -> ClientError {
    ClientError::Unexpected(format!("{:?}", message.message_type()))
}
