//! `prismd` — the Prism server binary.
//!
//! Usage:
//! - `prismd init <dir>` — create a database directory.
//! - `prismd run <dir> [bind_addr]` — open a database and serve it over TCP
//!   (default bind `0.0.0.0:4444`).
//!
//! See `docs/operations/build-and-dev.md`.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use prism_protocol::DEFAULT_PORT;
use prism_server::{Config, Instance, Server, ServerConfig, tls};

/// Install the tracing subscriber. Honors `RUST_LOG` (e.g. `RUST_LOG=info` or
/// `RUST_LOG=audit=info,prism_server=warn`); defaults to `info`. Audit events
/// are emitted on the `audit` target.
fn init_logging() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_target(true).try_init();
}

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("init") => match args.get(2) {
            Some(dir) => init(dir),
            None => usage(),
        },
        Some("run") => match args.get(2) {
            Some(dir) => {
                let mut bind = format!("0.0.0.0:{DEFAULT_PORT}");
                let mut tls_cert = None;
                let mut tls_key = None;
                let mut i = 3;
                while i < args.len() {
                    match args[i].as_str() {
                        "--tls-cert" => {
                            i += 1;
                            tls_cert = args.get(i).cloned();
                        }
                        "--tls-key" => {
                            i += 1;
                            tls_key = args.get(i).cloned();
                        }
                        a if !a.starts_with('-') => bind = a.to_string(),
                        other => {
                            eprintln!("prismd: unknown flag {other}");
                            return usage();
                        }
                    }
                    i += 1;
                }
                run(dir, &bind, tls_cert.as_deref(), tls_key.as_deref()).await
            }
            None => usage(),
        },
        _ => usage(),
    }
}

fn init(dir: &str) -> ExitCode {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("prismd: cannot create {dir}: {e}");
        return ExitCode::FAILURE;
    }
    match Instance::open_with(Path::new(dir), Config::durable()) {
        Ok(_) => {
            eprintln!("prismd: initialized data directory at {dir}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("prismd: init failed: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(dir: &str, bind: &str, tls_cert: Option<&str>, tls_key: Option<&str>) -> ExitCode {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("prismd: cannot create {dir}: {e}");
        return ExitCode::FAILURE;
    }
    let instance = match Instance::open_with(Path::new(dir), Config::durable()) {
        Ok(inst) => Arc::new(inst),
        Err(e) => {
            eprintln!("prismd: open {dir} failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let config = match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => {
            match tls::server_config_from_pem(Path::new(cert), Path::new(key)) {
                Ok(tls) => ServerConfig {
                    tls: Some(tls),
                    ..Default::default()
                },
                Err(e) => {
                    eprintln!("prismd: TLS configuration failed: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        (None, None) => ServerConfig::default(),
        _ => {
            eprintln!("prismd: --tls-cert and --tls-key must be given together");
            return ExitCode::FAILURE;
        }
    };
    let secure = config.tls.is_some();

    let server = match Server::bind_with(instance, bind, config).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("prismd: bind {bind} failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let addr = server
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| bind.to_string());
    tracing::info!(target: "prismd", %addr, tls = secure, "listening");
    if let Err(e) = server.run().await {
        tracing::error!(target: "prismd", error = %e, "server error");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn usage() -> ExitCode {
    eprintln!("usage: prismd <init|run> <dir> [bind_addr] [--tls-cert FILE --tls-key FILE]");
    ExitCode::FAILURE
}
