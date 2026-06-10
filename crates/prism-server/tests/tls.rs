//! End-to-end TLS: a TLS-wrapped server and the real client connecting over TLS
//! to run SQL — the same protocol, encrypted.
//!
//! Uses a static, long-lived self-signed certificate for `localhost` (embedded
//! below) so the tests need no certificate-generation dependency.

use std::sync::Arc;

use prism_client::Client;
use prism_protocol::Value;
use prism_server::{Database, Server, ServerConfig, tls};
use prism_testkit::TempDir;
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// A self-signed certificate for `localhost`, valid 2020-01-01 .. 4096-01-01.
const CERT_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIIBXzCCAQSgAwIBAgIUHk91XrtmtiQqYrfwzHUyLK2h2AAwCgYIKoZIzj0EAwIw
ITEfMB0GA1UEAwwWcmNnZW4gc2VsZiBzaWduZWQgY2VydDAgFw0yMDAxMDEwMDAw
MDBaGA80MDk2MDEwMTAwMDAwMFowITEfMB0GA1UEAwwWcmNnZW4gc2VsZiBzaWdu
ZWQgY2VydDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABBiAkAriuXmM/v/DVXpb
Wtdh1jjyAM5JMkCuxnX3uZXRBTrer8GEYFxdAk1XuXkTzC2eCDeDrdLydTLvg9le
y/2jGDAWMBQGA1UdEQQNMAuCCWxvY2FsaG9zdDAKBggqhkjOPQQDAgNJADBGAiEA
8SB99fQTYscJO/2ON4TbdkIKZ5fWmo1hLU/9pOd9DWsCIQDjXw4Us4PpZVoPaSCc
cyPugjMpK8Yj/PD1qEe8U22WHg==
-----END CERTIFICATE-----
";

const KEY_PEM: &str = "\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgX3N7wOJFHxW8mm3R
7irwUYzVbbtYSyg/TBBKgFFc6/6hRANCAAQYgJAK4rl5jP7/w1V6W1rXYdY48gDO
STJArsZ197mV0QU63q/BhGBcXQJNV7l5E8wtngg3g63S8nUy74PZXsv9
-----END PRIVATE KEY-----
";

fn cert() -> CertificateDer<'static> {
    CertificateDer::from_pem_slice(CERT_PEM.as_bytes()).unwrap()
}

fn key() -> PrivateKeyDer<'static> {
    PrivateKeyDer::from_pem_slice(KEY_PEM.as_bytes()).unwrap()
}

async fn tls_server() -> std::net::SocketAddr {
    let server_tls = tls::server_config(vec![cert()], key()).unwrap();
    let tmp = TempDir::new("tls").unwrap();
    let db = Arc::new(Database::open(tmp.path()).unwrap());
    let server = Server::bind_with(
        db,
        "127.0.0.1:0",
        ServerConfig {
            tls: Some(server_tls),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr().unwrap();
    // Keep the temp dir alive for the server's lifetime.
    tokio::spawn(async move {
        let _tmp = tmp;
        let _ = server.run().await;
    });
    addr
}

#[tokio::test]
async fn tls_handshake_then_sql_roundtrip() {
    let addr = tls_server().await;

    // A client that trusts the self-signed certificate, connecting over TLS.
    let mut roots = RootCertStore::empty();
    roots.add(cert()).unwrap();
    let mut c = Client::connect_tls(addr, "localhost", prism_client::tls_client_config(roots))
        .await
        .unwrap();

    // Authenticates over TLS (the admin OID is an internal id, not asserted).
    c.authenticate("admin", "admin").await.unwrap();
    c.sql("CREATE TABLE t (id BIGINT NOT NULL, name TEXT)")
        .await
        .unwrap();
    assert_eq!(
        c.sql("INSERT INTO t VALUES (1,'over-tls')")
            .await
            .unwrap()
            .affected,
        1
    );
    let rows = c.sql("SELECT id, name FROM t").await.unwrap().rows;
    assert_eq!(
        rows,
        vec![vec![
            Some(Value::Int64(1)),
            Some(Value::Str("over-tls".into()))
        ]]
    );
}

#[tokio::test]
async fn plaintext_client_cannot_talk_to_a_tls_server() {
    let addr = tls_server().await;
    // A plaintext frame is not a valid TLS record; the connection fails rather
    // than serving requests.
    assert!(Client::connect(addr).await.is_err());
}
