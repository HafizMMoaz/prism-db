//! The Tokio TCP front-end: an accept loop and a per-connection framing loop
//! that pumps wire frames through an authenticating [`Session`].
//!
//! The dispatcher ([`Session`]) is synchronous and transport-agnostic, so this
//! layer is thin: accept a connection (subject to a connection cap), then loop
//! reading length-prefixed frames ([`prism_protocol::frame`]), decode each to a
//! [`Packet`], run it through the session, and write the response frame back.
//! The session enforces the `Hello` → `Auth` handshake; when it signals
//! [`Session::is_closing`] (version mismatch, bad credentials, protocol
//! violation) the connection is dropped after the reply. Dropping the connection
//! drops the session, which aborts any open transaction.
//!
//! Enforced here: a maximum connection count and a per-connection idle timeout.
//! **Deferred** (`docs/components/network-server.md`): TLS, per-user limits,
//! idempotency, cancellation, and graceful draining; dispatch runs inline on the
//! connection task (`spawn_blocking` offload is a refinement for when fsync is
//! enabled).

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use prism_protocol::{
    DEFAULT_IDLE_TIMEOUT_SECS, Message, NoticeSeverity, Packet, ProtocolError, frame,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_rustls::TlsAcceptor;

use crate::database::Database;
use crate::session::Session;

/// Default maximum simultaneous connections.
pub const DEFAULT_MAX_CONNECTIONS: usize = 10_000;

/// Network-server tuning.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Maximum simultaneous connections; further connects are rejected.
    pub max_connections: usize,
    /// How long a connection may be idle before the server closes it.
    pub idle_timeout: Duration,
    /// TLS configuration. When `Some`, connections are wrapped in TLS before the
    /// first frame; when `None`, the server speaks plaintext.
    pub tls: Option<Arc<rustls::ServerConfig>>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS as u64),
            tls: None,
        }
    }
}

/// A bound TCP server over a shared [`Database`].
pub struct Server {
    listener: TcpListener,
    db: Arc<Database>,
    config: ServerConfig,
    active: Arc<AtomicUsize>,
}

impl Server {
    /// Bind to `addr` with default [`ServerConfig`].
    pub async fn bind(db: Arc<Database>, addr: impl ToSocketAddrs) -> io::Result<Self> {
        Self::bind_with(db, addr, ServerConfig::default()).await
    }

    /// Bind to `addr` with explicit [`ServerConfig`].
    pub async fn bind_with(
        db: Arc<Database>,
        addr: impl ToSocketAddrs,
        config: ServerConfig,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Self {
            listener,
            db,
            config,
            active: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// The actual local address (useful when binding to port 0 in tests).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept connections forever, serving each on its own task.
    pub async fn run(self) -> io::Result<()> {
        loop {
            let (stream, _peer) = self.listener.accept().await?;

            // Enforce the connection cap before spending resources on a task.
            if self.active.fetch_add(1, Ordering::AcqRel) >= self.config.max_connections {
                self.active.fetch_sub(1, Ordering::AcqRel);
                tokio::spawn(reject_overloaded(stream));
                continue;
            }

            let guard = ConnGuard(self.active.clone());
            let db = self.db.clone();
            let idle = self.config.idle_timeout;
            let tls = self.config.tls.clone();
            tokio::spawn(async move {
                let _guard = guard; // decrements the active count on completion
                match tls {
                    // Wrap in TLS before the first frame, then serve.
                    Some(cfg) => {
                        if let Ok(tls_stream) = TlsAcceptor::from(cfg).accept(stream).await {
                            let _ = serve_connection(tls_stream, db, idle).await;
                        }
                    }
                    None => {
                        let _ = serve_connection(stream, db, idle).await;
                    }
                }
            });
        }
    }
}

/// Decrements the active-connection count when a connection task ends.
struct ConnGuard(Arc<AtomicUsize>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Run one connection's request loop until EOF, a handshake close, an idle
/// timeout, a protocol violation, or an I/O error. The session (and thus any
/// open transaction) is dropped on return.
async fn serve_connection<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    db: Arc<Database>,
    idle_timeout: Duration,
) -> io::Result<()> {
    let mut session = Session::new_authenticating(db);
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];

    loop {
        // Drain every complete frame currently buffered before reading more.
        loop {
            let (decoded, consumed) = match frame::parse(&buf) {
                Ok(Some((payload, consumed))) => (Packet::from_payload(payload), consumed),
                Ok(None) => break, // need more bytes
                // An over-large declared frame is a protocol violation: close.
                Err(_) => return Ok(()),
            };
            buf.drain(..consumed);

            match decoded {
                Ok(request) => {
                    let response = session.handle_packet(request);
                    let bytes = response.to_frame().map_err(encode_error)?;
                    stream.write_all(&bytes).await?;
                    if session.is_closing() {
                        stream.flush().await?;
                        return Ok(()); // failed handshake / protocol violation
                    }
                }
                Err(e) => {
                    if let Ok(bytes) =
                        notice(NoticeSeverity::Error, 0x0001, &e.to_string()).to_frame()
                    {
                        let _ = stream.write_all(&bytes).await;
                    }
                    return Ok(());
                }
            }
        }

        stream.flush().await?;
        match tokio::time::timeout(idle_timeout, stream.read(&mut chunk)).await {
            Ok(result) => {
                let n = result?;
                if n == 0 {
                    return Ok(()); // clean EOF
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(_elapsed) => {
                // Idle too long: warn and close.
                if let Ok(bytes) =
                    notice(NoticeSeverity::Warning, 0x0001, "idle timeout").to_frame()
                {
                    let _ = stream.write_all(&bytes).await;
                    let _ = stream.flush().await;
                }
                return Ok(());
            }
        }
    }
}

/// Tell a rejected client the server is at capacity, then drop the connection.
async fn reject_overloaded(mut stream: TcpStream) {
    if let Ok(bytes) = notice(NoticeSeverity::Error, 0x0601, "too many connections").to_frame() {
        let _ = stream.write_all(&bytes).await;
        let _ = stream.flush().await;
    }
}

/// A server-initiated notice (`request_id` 0).
fn notice(severity: NoticeSeverity, code: u32, message: &str) -> Packet {
    Packet::new(
        0,
        Message::Notice {
            severity,
            code,
            message: message.to_string(),
        },
    )
}

fn encode_error(e: ProtocolError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}
