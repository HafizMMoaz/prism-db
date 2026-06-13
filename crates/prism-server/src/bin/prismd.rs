//! `prismd` — the Prism server binary: a multi-database server over one data
//! directory (each database is a subdirectory; `_system` holds the accounts).
//!
//! Usage:
//! - `prismd init [data-dir]` — create/initialize the data directory.
//! - `prismd run [data-dir] [bind] [--data DIR] [--bind ADDR]
//!   [--tls-cert FILE --tls-key FILE]` — serve over TCP.
//!
//! The data directory defaults to `$PRISM_DATA_DIR`, else a platform location
//! (`%ProgramData%\PrismDB\data` on Windows, `/var/lib/prismdb` on Linux when it
//! exists, else `~/.prismdb`). Bind defaults to `0.0.0.0:4444`.
//!
//! See `docs/operations/install.md`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use prism_protocol::DEFAULT_PORT;
use prism_server::{Config, Instance, Server, ServerConfig, tls};

/// The data directory: `$PRISM_DATA_DIR`, else a platform default.
fn default_data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PRISM_DATA_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    #[cfg(windows)]
    if let Ok(pd) = std::env::var("ProgramData") {
        if !pd.is_empty() {
            return PathBuf::from(pd).join("PrismDB").join("data");
        }
    }
    #[cfg(not(windows))]
    {
        let sys = PathBuf::from("/var/lib/prismdb");
        if sys.is_dir() {
            return sys;
        }
    }
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        if !home.is_empty() {
            return PathBuf::from(home).join(".prismdb");
        }
    }
    PathBuf::from("prism-data")
}

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
        Some("init") => {
            let dir = args
                .get(2)
                .filter(|a| !a.starts_with('-'))
                .map(PathBuf::from)
                .unwrap_or_else(default_data_dir);
            init(&dir)
        }
        Some("run") => {
            let mut data: Option<String> = None;
            let mut bind: Option<String> = None;
            let mut tls_cert = None;
            let mut tls_key = None;
            let mut positionals: Vec<&str> = Vec::new();
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--data" => {
                        i += 1;
                        data = args.get(i).cloned();
                    }
                    "--bind" => {
                        i += 1;
                        bind = args.get(i).cloned();
                    }
                    "--tls-cert" => {
                        i += 1;
                        tls_cert = args.get(i).cloned();
                    }
                    "--tls-key" => {
                        i += 1;
                        tls_key = args.get(i).cloned();
                    }
                    a if !a.starts_with('-') => positionals.push(a),
                    other => {
                        eprintln!("prismd: unknown flag {other}");
                        return usage();
                    }
                }
                i += 1;
            }
            // Legacy positionals: <data-dir> [bind].
            let data_dir = data
                .map(PathBuf::from)
                .or_else(|| positionals.first().map(PathBuf::from))
                .unwrap_or_else(default_data_dir);
            let bind = bind
                .or_else(|| positionals.get(1).map(|s| s.to_string()))
                .unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_PORT}"));
            run(&data_dir, &bind, tls_cert.as_deref(), tls_key.as_deref()).await
        }
        _ => usage(),
    }
}

fn init(dir: &Path) -> ExitCode {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("prismd: cannot create {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    match Instance::open_with(dir, Config::durable()) {
        Ok(_) => {
            eprintln!("prismd: initialized data directory at {}", dir.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("prismd: init failed: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(dir: &Path, bind: &str, tls_cert: Option<&str>, tls_key: Option<&str>) -> ExitCode {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("prismd: cannot create {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    let instance = match Instance::open_with(dir, Config::durable()) {
        Ok(inst) => Arc::new(inst),
        Err(e) => {
            eprintln!("prismd: open {} failed: {e}", dir.display());
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
    tracing::info!(target: "prismd", %addr, tls = secure, data = %dir.display(), "listening");
    if let Err(e) = server.run().await {
        tracing::error!(target: "prismd", error = %e, "server error");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  \
         prismd init [data-dir]\n  \
         prismd run  [data-dir] [bind] [--data DIR] [--bind ADDR] \
         [--tls-cert FILE --tls-key FILE]\n\n\
         data-dir defaults to $PRISM_DATA_DIR or a platform location; \
         bind defaults to 0.0.0.0:4444."
    );
    ExitCode::FAILURE
}
