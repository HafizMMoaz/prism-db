//! TLS configuration for the server (rustls, ring crypto backend).
//!
//! TLS 1.2/1.3 over the same length-prefixed protocol — the connection is
//! wrapped before the first frame is read. The server presents a certificate;
//! clients verify it. mTLS (client certificates) is a follow-up. See
//! `docs/components/network-server.md`.

use std::io;
use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// Build a rustls server config from an in-memory certificate chain and key.
pub fn server_config(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<ServerConfig>, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(Arc::new(config))
}

/// Load a rustls server config from PEM certificate and private-key files.
pub fn server_config_from_pem(cert_path: &Path, key_path: &Path) -> io::Result<Arc<ServerConfig>> {
    let certs = CertificateDer::pem_file_iter(cert_path)
        .map_err(to_io)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(to_io)?;
    let key = PrivateKeyDer::from_pem_file(key_path).map_err(to_io)?;
    server_config(certs, key).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

fn to_io(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}
