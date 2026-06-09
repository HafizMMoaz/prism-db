//! The Tokio TCP front-end: an accept loop and a per-connection framing loop
//! that pumps wire frames through a [`Session`].
//!
//! The dispatcher ([`Session`]) is synchronous and transport-agnostic, so this
//! layer is thin: accept a connection, then loop reading length-prefixed frames
//! ([`prism_protocol::frame`]), decode each to a [`Packet`], run it through the
//! session, and write the response frame back. The `Hello`/`Auth` handshake is
//! just the first few requests the session answers — no special-casing here.
//! Dropping the connection drops the session, which aborts any open transaction
//! (see `Session`'s `Drop`).
//!
//! **Deferred (this increment):** TLS, the connection/transaction limits,
//! idempotency, cancellation, and graceful draining from
//! `docs/components/network-server.md`. Dispatch runs inline on the connection
//! task; offloading the (currently in-memory, fast) engine work to
//! `spawn_blocking`/`block_in_place` is a refinement for when fsync is enabled.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use prism_protocol::{Message, NoticeSeverity, Packet, ProtocolError, frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};

use crate::database::Database;
use crate::session::Session;

/// A bound TCP server over a shared [`Database`].
pub struct Server {
    listener: TcpListener,
    db: Arc<Database>,
}

impl Server {
    /// Bind to `addr` and prepare to serve `db`.
    pub async fn bind(db: Arc<Database>, addr: impl ToSocketAddrs) -> io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Self { listener, db })
    }

    /// The actual local address (useful when binding to port 0 in tests).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept connections forever, serving each on its own task.
    pub async fn run(self) -> io::Result<()> {
        loop {
            let (stream, _peer) = self.listener.accept().await?;
            let db = self.db.clone();
            tokio::spawn(async move {
                // A connection error (reset, malformed frame) just ends that
                // connection; the server keeps accepting others.
                let _ = serve_connection(stream, db).await;
            });
        }
    }
}

/// Run one connection's request loop until EOF, a protocol violation, or an I/O
/// error. The session (and thus any open transaction) is dropped on return.
async fn serve_connection(mut stream: TcpStream, db: Arc<Database>) -> io::Result<()> {
    let mut session = Session::new(db);
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];

    loop {
        // Drain every complete frame currently buffered before reading more.
        loop {
            let (decoded, consumed) = match frame::parse(&buf) {
                Ok(Some((payload, consumed))) => (Packet::from_payload(payload), consumed),
                Ok(None) => break, // need more bytes
                // An over-large declared frame is a protocol violation: close
                // the connection with no response (per the wire spec).
                Err(_) => return Ok(()),
            };
            buf.drain(..consumed);

            match decoded {
                Ok(request) => {
                    let response = session.handle_packet(request);
                    let bytes = response.to_frame().map_err(encode_error)?;
                    stream.write_all(&bytes).await?;
                }
                Err(e) => {
                    // A malformed payload: send a best-effort Notice, then close.
                    if let Ok(bytes) = notice(&e).to_frame() {
                        let _ = stream.write_all(&bytes).await;
                    }
                    return Ok(());
                }
            }
        }

        stream.flush().await?;
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(()); // clean EOF
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// A server-initiated error notice (`request_id` 0).
fn notice(e: &ProtocolError) -> Packet {
    Packet::new(
        0,
        Message::Notice {
            severity: NoticeSeverity::Error,
            code: 0x0001,
            message: e.to_string(),
        },
    )
}

fn encode_error(e: ProtocolError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}
