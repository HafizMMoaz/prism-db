//! `prism-shell` — the interactive client REPL.
//!
//! Connects to a Prism server, authenticates, and runs a read-eval-print loop:
//! SQL statements and backslash commands (see [`prism_shell::help_text`]) go to
//! the server via [`prism_client`], and results are formatted per model. See
//! `docs/specs/shell.md`.
//!
//! Usage: `prism-shell [host:port] [--user U] [--password P]`
//! (defaults: `127.0.0.1:4444`, `admin`/`admin`).

use std::io::Write as _;
use std::process::ExitCode;

use prism_client::Client;
use prism_doc::Document;
use prism_protocol::DEFAULT_PORT;
use prism_shell::{Command, help_text, hex, parse_command, render_document, render_query};
use tokio::io::{AsyncBufReadExt, BufReader};

struct Config {
    addr: String,
    user: String,
    password: String,
}

#[tokio::main]
async fn main() -> ExitCode {
    let config = match parse_args() {
        Ok(c) => c,
        Err(message) => {
            eprintln!("prism-shell: {message}");
            return ExitCode::FAILURE;
        }
    };

    let mut client =
        match Client::connect_authenticated(&config.addr, &config.user, &config.password).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("prism-shell: cannot connect to {}: {e}", config.addr);
                return ExitCode::FAILURE;
            }
        };
    eprintln!(
        "connected to {} as {}. \\help for commands, \\quit to exit.",
        config.addr, config.user
    );

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        prompt();
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break, // EOF (Ctrl-D / end of piped input)
            Err(e) => {
                eprintln!("read error: {e}");
                break;
            }
        };
        let command = match parse_command(&line) {
            Ok(c) => c,
            Err(message) => {
                println!("error: {message}");
                continue;
            }
        };
        match execute(&mut client, command).await {
            Ok(true) => {}
            Ok(false) => break, // \quit
            Err(message) => println!("error: {message}"),
        }
    }
    eprintln!("bye");
    ExitCode::SUCCESS
}

/// Execute one command. Returns `Ok(false)` to quit the loop.
async fn execute(client: &mut Client, command: Command) -> Result<bool, String> {
    match command {
        Command::Empty => {}
        Command::Quit => return Ok(false),
        Command::Help => println!("{}", help_text()),
        Command::Ping => {
            client.ping().await.map_err(stringify)?;
            println!("pong");
        }
        Command::Begin => {
            let id = client.begin(false).await.map_err(stringify)?;
            println!("BEGIN (txn {id})");
        }
        Command::Commit => {
            client.commit().await.map_err(stringify)?;
            println!("COMMIT");
        }
        Command::Abort => {
            client.abort().await.map_err(stringify)?;
            println!("ABORT");
        }
        Command::Sql(sql) => {
            let result = client.sql(&sql).await.map_err(stringify)?;
            println!("{}", render_query(&result));
        }
        Command::KvGet { ns, key } => match client
            .kv_get(&ns, key.as_bytes())
            .await
            .map_err(stringify)?
        {
            Some(value) => println!("{}", String::from_utf8_lossy(&value)),
            None => println!("(nil)"),
        },
        Command::KvPut { ns, key, value } => {
            client
                .kv_put(&ns, key.as_bytes(), value.as_bytes())
                .await
                .map_err(stringify)?;
            println!("OK");
        }
        Command::KvDel { ns, key } => {
            client
                .kv_delete(&ns, key.as_bytes())
                .await
                .map_err(stringify)?;
            println!("OK");
        }
        Command::DocFind { collection } => {
            let query = Document::new().encode().map_err(stringify)?;
            let reply = client
                .doc_find(&collection, query)
                .await
                .map_err(stringify)?;
            for doc in &reply.docs {
                println!("{}", render_document(doc));
            }
            println!("({} document(s))", reply.docs.len());
        }
        Command::DocInsert { collection, fields } => {
            let bytes = Document::from_fields(fields).encode().map_err(stringify)?;
            let reply = client
                .doc_insert_one(&collection, bytes)
                .await
                .map_err(stringify)?;
            match reply.inserted_ids.first() {
                Some(id) => println!("inserted _id {}", hex(id)),
                None => println!("inserted {} document(s)", reply.affected),
            }
        }
    }
    Ok(true)
}

fn stringify(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn prompt() {
    let mut err = std::io::stderr();
    let _ = write!(err, "prism> ");
    let _ = err.flush();
}

fn parse_args() -> Result<Config, String> {
    let mut config = Config {
        addr: format!("127.0.0.1:{DEFAULT_PORT}"),
        user: "admin".to_string(),
        password: "admin".to_string(),
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--user" | "-u" => {
                i += 1;
                config.user = args.get(i).ok_or("--user needs a value")?.clone();
            }
            "--password" | "-p" => {
                i += 1;
                config.password = args.get(i).ok_or("--password needs a value")?.clone();
            }
            "--help" | "-h" => {
                return Err("usage: prism-shell [host:port] [--user U] [--password P]".into());
            }
            addr if !addr.starts_with('-') => config.addr = addr.to_string(),
            other => return Err(format!("unknown flag {other}")),
        }
        i += 1;
    }
    Ok(config)
}
