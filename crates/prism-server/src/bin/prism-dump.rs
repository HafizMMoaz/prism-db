//! `prism-dump` — logical backup/restore for a PrismDB directory.
//!
//!   prism-dump export <dir> [file]   dump structure + data (stdout if no file)
//!   prism-dump import <dir> <file>   restore a dump into the database
//!
//! The dump is a consistent point-in-time snapshot across all three models:
//! tables as `CREATE TABLE`/`INSERT` SQL, documents and KV pairs as hex lines.

use std::path::Path;
use std::process::ExitCode;

use prism_server::{Config, Database};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("export") => match args.get(2) {
            Some(dir) => export(dir, args.get(3).map(String::as_str)),
            None => usage(),
        },
        Some("import") => match (args.get(2), args.get(3)) {
            (Some(dir), Some(file)) => import(dir, file),
            _ => usage(),
        },
        _ => usage(),
    }
}

fn open(dir: &str) -> Result<Database, ExitCode> {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("prism-dump: cannot create {dir}: {e}");
        return Err(ExitCode::FAILURE);
    }
    Database::open_with(Path::new(dir), Config::durable()).map_err(|e| {
        eprintln!("prism-dump: open {dir} failed: {e}");
        ExitCode::FAILURE
    })
}

fn export(dir: &str, file: Option<&str>) -> ExitCode {
    let db = match open(dir) {
        Ok(db) => db,
        Err(code) => return code,
    };
    let dump = match prism_server::export_to_string(&db) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("prism-dump: export failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    match file {
        Some(path) => {
            if let Err(e) = std::fs::write(path, &dump) {
                eprintln!("prism-dump: cannot write {path}: {e}");
                return ExitCode::FAILURE;
            }
            eprintln!("prism-dump: wrote {} bytes to {path}", dump.len());
        }
        None => print!("{dump}"),
    }
    ExitCode::SUCCESS
}

fn import(dir: &str, file: &str) -> ExitCode {
    let dump = match std::fs::read_to_string(file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("prism-dump: cannot read {file}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let db = match open(dir) {
        Ok(db) => db,
        Err(code) => return code,
    };
    match prism_server::import(&db, &dump) {
        Ok(stats) => {
            eprintln!(
                "prism-dump: imported {} table(s), {} row(s), {} document(s), {} kv pair(s)",
                stats.tables, stats.rows, stats.documents, stats.kv_pairs
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("prism-dump: import failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: prism-dump <export <dir> [file] | import <dir> <file>>");
    ExitCode::FAILURE
}
