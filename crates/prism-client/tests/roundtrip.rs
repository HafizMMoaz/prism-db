//! Client round-trips against a hand-rolled mock server that speaks the wire
//! protocol - so the client is exercised without depending on `prism-server`.

use std::net::SocketAddr;

use prism_client::{Client, ClientError};
use prism_protocol::{
    ColumnDesc, ErrorInfo, KvCommand, KvResultBody, Message, NoticeSeverity, Packet, Value,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Spawn a mock server that answers each request with a canned response and
/// returns its address.
async fn mock_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        while let Some(packet) = read_packet(&mut stream).await {
            // Before answering a Ping, send an unsolicited notice (request_id 0)
            // - the client must skip it and still match the Pong.
            if matches!(packet.message, Message::Ping) {
                let notice = Packet::new(
                    0,
                    Message::Notice {
                        severity: NoticeSeverity::Info,
                        code: 7,
                        message: "hello from the server".into(),
                    },
                );
                write_packet(&mut stream, notice).await;
            }
            let response = respond(packet.message);
            write_packet(&mut stream, Packet::new(packet.request_id, response)).await;
        }
    });
    addr
}

fn respond(request: Message) -> Message {
    match request {
        Message::Hello { .. } => Message::HelloAck {
            status: 0,
            server_version: "mock".into(),
            features: 0,
            session_id: 1,
            error: None,
        },
        Message::Auth { password, .. } => {
            if password == "good" {
                Message::AuthAck {
                    status: 0,
                    user_oid: 42,
                    error: None,
                }
            } else {
                Message::AuthAck {
                    status: 1,
                    user_oid: 0,
                    error: Some(ErrorInfo {
                        error_code: 0x0100,
                        message: "bad credentials".into(),
                        sqlstate: *b"28000",
                        detail: String::new(),
                        position: 0,
                    }),
                }
            }
        }
        Message::Ping => Message::Pong,
        Message::Begin { .. } | Message::Commit { .. } | Message::Abort => Message::TxnAck {
            status: 0,
            txn_id: 100,
            commit_lsn: 0,
            error: None,
        },
        Message::SqlExecute { sql, .. } => {
            if sql.contains("BADTABLE") {
                Message::SqlResult {
                    status: 1,
                    affected_rows: 0,
                    columns: vec![],
                    rows: vec![],
                    more_frames: false,
                    error: Some(ErrorInfo {
                        error_code: 0x0400,
                        message: "no such table".into(),
                        sqlstate: *b"42P01",
                        detail: String::new(),
                        position: 0,
                    }),
                }
            } else if sql.starts_with("SELECT") {
                Message::SqlResult {
                    status: 0,
                    affected_rows: 0,
                    columns: vec![ColumnDesc {
                        name: "id".into(),
                        type_tag: 0x03,
                        nullable: false,
                    }],
                    rows: vec![vec![Some(Value::Int64(1))], vec![Some(Value::Int64(2))]],
                    more_frames: false,
                    error: None,
                }
            } else {
                Message::SqlResult {
                    status: 0,
                    affected_rows: 1,
                    columns: vec![],
                    rows: vec![],
                    more_frames: false,
                    error: None,
                }
            }
        }
        Message::KvOp { command, .. } => {
            let body = match command {
                KvCommand::Get { key } if key == b"present" => KvResultBody::Get {
                    value: Some(b"value".to_vec()),
                },
                KvCommand::Get { .. } => KvResultBody::Get { value: None },
                KvCommand::Put { .. } => KvResultBody::Put,
                KvCommand::Delete { .. } => KvResultBody::Delete,
                KvCommand::Range { .. } => KvResultBody::Range {
                    entries: vec![],
                    more_frames: false,
                },
                KvCommand::Scan { .. } => KvResultBody::Scan {
                    entries: vec![],
                    more_frames: false,
                },
            };
            Message::KvResult {
                status: 0,
                body,
                error: None,
            }
        }
        Message::DocOp { command, .. } => match command {
            prism_protocol::DocCommand::InsertOne(_) => Message::DocResult {
                status: 0,
                affected: 1,
                inserted_ids: vec![[9u8; 12]],
                docs: vec![],
                more_frames: false,
                error: None,
            },
            _ => Message::DocResult {
                status: 0,
                affected: 1,
                inserted_ids: vec![],
                docs: vec![vec![1, 2, 3]],
                more_frames: false,
                error: None,
            },
        },
        other => Message::Notice {
            severity: NoticeSeverity::Error,
            code: 1,
            message: format!("mock can't handle {:?}", other.message_type()),
        },
    }
}

async fn read_packet(stream: &mut TcpStream) -> Option<Packet> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await.ok()?;
    let mut payload = vec![0u8; u32::from_le_bytes(len) as usize];
    stream.read_exact(&mut payload).await.ok()?;
    Some(Packet::from_payload(&payload).unwrap())
}

async fn write_packet(stream: &mut TcpStream, packet: Packet) {
    stream.write_all(&packet.to_frame().unwrap()).await.unwrap();
    stream.flush().await.unwrap();
}

#[tokio::test]
async fn connect_authenticate_ping() {
    let addr = mock_server().await;
    let mut c = Client::connect(addr).await.unwrap();
    assert_eq!(c.authenticate("admin", "good").await.unwrap(), 42);
    c.ping().await.unwrap(); // also exercises skipping an unsolicited notice
}

#[tokio::test]
async fn bad_credentials_are_reported() {
    let addr = mock_server().await;
    let mut c = Client::connect(addr).await.unwrap();
    assert!(matches!(
        c.authenticate("admin", "nope").await,
        Err(ClientError::AuthFailed)
    ));
}

#[tokio::test]
async fn sql_select_and_insert() {
    let addr = mock_server().await;
    let mut c = Client::connect_authenticated(addr, "admin", "good")
        .await
        .unwrap();

    let select = c.sql("SELECT id FROM t").await.unwrap();
    assert_eq!(select.columns.len(), 1);
    assert_eq!(select.rows.len(), 2);
    assert_eq!(select.rows[0][0], Some(Value::Int64(1)));

    let insert = c.sql("INSERT INTO t VALUES (1)").await.unwrap();
    assert_eq!(insert.affected, 1);
    assert!(insert.rows.is_empty());
}

#[tokio::test]
async fn server_error_surfaces() {
    let addr = mock_server().await;
    let mut c = Client::connect_authenticated(addr, "admin", "good")
        .await
        .unwrap();
    match c.sql("SELECT * FROM BADTABLE").await {
        Err(ClientError::Server { code, sqlstate, .. }) => {
            assert_eq!(code, 0x0400);
            assert_eq!(sqlstate, "42P01");
        }
        other => panic!("expected a server error, got {other:?}"),
    }
}

#[tokio::test]
async fn kv_and_transaction_and_doc() {
    let addr = mock_server().await;
    let mut c = Client::connect_authenticated(addr, "admin", "good")
        .await
        .unwrap();

    assert_eq!(c.begin(false).await.unwrap(), 100);
    c.kv_put("ns", b"k", b"v").await.unwrap();
    assert_eq!(
        c.kv_get("ns", b"present").await.unwrap().as_deref(),
        Some(&b"value"[..])
    );
    assert_eq!(c.kv_get("ns", b"absent").await.unwrap(), None);
    c.commit().await.unwrap();

    let inserted = c.doc_insert_one("coll", vec![0xAA]).await.unwrap();
    assert_eq!(inserted.inserted_ids, vec![[9u8; 12]]);
    let found = c.doc_find("coll", vec![]).await.unwrap();
    assert_eq!(found.docs, vec![vec![1, 2, 3]]);
}
