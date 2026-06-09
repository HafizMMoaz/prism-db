//! End-to-end tests over a real TCP socket: the framing loop + handshake +
//! cross-model transactions, exactly as a client would drive them.

use std::sync::Arc;

use prism_doc::{DocValue, Document};
use prism_protocol::{
    AuthMechanism, DocCommand, KvCommand, KvResultBody, Message, PROTOCOL_VERSION, Packet, TxnMode,
    Value as WireValue,
};
use prism_server::{Database, Server};
use prism_testkit::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A tiny test client: frames a request and reads one framed response.
struct Client {
    stream: TcpStream,
    next_id: u32,
}

impl Client {
    async fn connect(addr: std::net::SocketAddr) -> Self {
        Self {
            stream: TcpStream::connect(addr).await.unwrap(),
            next_id: 1,
        }
    }

    async fn request(&mut self, message: Message) -> Message {
        let id = self.next_id;
        self.next_id += 1;
        let frame = Packet::new(id, message).to_frame().unwrap();
        self.stream.write_all(&frame).await.unwrap();
        self.stream.flush().await.unwrap();

        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len).await.unwrap();
        let mut payload = vec![0u8; u32::from_le_bytes(len) as usize];
        self.stream.read_exact(&mut payload).await.unwrap();
        let packet = Packet::from_payload(&payload).unwrap();
        assert_eq!(packet.request_id, id, "response echoes the request id");
        packet.message
    }

    /// Complete the `Hello` → `Auth` handshake with the default admin account.
    async fn login(&mut self) {
        assert!(matches!(
            self.request(Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                client_name: "itest".into(),
                client_version: "0".into(),
                features: 0,
            })
            .await,
            Message::HelloAck { status: 0, .. }
        ));
        assert!(matches!(
            self.request(Message::Auth {
                mechanism: AuthMechanism::Password,
                username: "admin".into(),
                password: "admin".into(),
            })
            .await,
            Message::AuthAck { status: 0, .. }
        ));
    }
}

async fn start_server() -> (std::net::SocketAddr, TempDir) {
    let tmp = TempDir::new("net").unwrap();
    let db = Arc::new(Database::open(tmp.path()).unwrap());
    let server = Server::bind(db, "127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(server.run());
    (addr, tmp)
}

fn sql(s: &str) -> Message {
    Message::SqlExecute {
        sql: s.into(),
        params: vec![],
        options: 1,
    }
}

#[tokio::test]
async fn handshake_then_sql_over_tcp() {
    let (addr, _tmp) = start_server().await;
    let mut c = Client::connect(addr).await;

    c.login().await;
    assert!(matches!(c.request(Message::Ping).await, Message::Pong));

    c.request(sql("CREATE TABLE t (id BIGINT NOT NULL, name TEXT)"))
        .await;
    match c.request(sql("INSERT INTO t VALUES (1,'alice')")).await {
        Message::SqlResult {
            status: 0,
            affected_rows,
            ..
        } => assert_eq!(affected_rows, 1),
        other => panic!("expected SqlResult, got {other:?}"),
    }
    match c.request(sql("SELECT id, name FROM t")).await {
        Message::SqlResult {
            status: 0, rows, ..
        } => {
            assert_eq!(
                rows,
                vec![vec![
                    Some(WireValue::Int64(1)),
                    Some(WireValue::Str("alice".into()))
                ]]
            );
        }
        other => panic!("expected SqlResult, got {other:?}"),
    }
}

#[tokio::test]
async fn explicit_cross_model_transaction_over_tcp() {
    let (addr, _tmp) = start_server().await;
    let mut c = Client::connect(addr).await;
    c.login().await;

    c.request(sql("CREATE TABLE accounts (id BIGINT NOT NULL)"))
        .await;

    // One explicit transaction, three models, over the wire.
    assert!(matches!(
        c.request(Message::Begin {
            mode: TxnMode::ReadWrite
        })
        .await,
        Message::TxnAck { status: 0, .. }
    ));
    c.request(sql("INSERT INTO accounts VALUES (1)")).await;

    let doc = Document::from_fields([("acct".to_string(), DocValue::Int64(1))]);
    c.request(Message::DocOp {
        collection: "audit".into(),
        command: DocCommand::InsertOne(doc.encode().unwrap()),
    })
    .await;

    assert!(matches!(
        c.request(Message::KvOp {
            namespace: "bal".into(),
            command: KvCommand::Put {
                key: b"acct:1".to_vec(),
                value: b"100".to_vec(),
            },
        })
        .await,
        Message::KvResult {
            status: 0,
            body: KvResultBody::Put,
            ..
        }
    ));

    assert!(matches!(
        c.request(Message::Commit { idempotency_key: 0 }).await,
        Message::TxnAck { status: 0, .. }
    ));

    // A second connection sees all three committed writes.
    let mut c2 = Client::connect(addr).await;
    c2.login().await;
    match c2.request(sql("SELECT id FROM accounts")).await {
        Message::SqlResult { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("expected SqlResult, got {other:?}"),
    }
    match c2
        .request(Message::KvOp {
            namespace: "bal".into(),
            command: KvCommand::Get {
                key: b"acct:1".to_vec(),
            },
        })
        .await
    {
        Message::KvResult {
            body: KvResultBody::Get { value },
            ..
        } => assert_eq!(value.as_deref(), Some(&b"100"[..])),
        other => panic!("expected KvResult, got {other:?}"),
    }
}
