//! `prism-shell` library: the command parser and per-model output formatters.
//!
//! Kept separate from the binary so they can be unit-tested without a live
//! connection. A line is either a SQL statement or a backslash command
//! (`\help`, `\ping`, `\begin`/`\commit`/`\abort`, `\kv …`, `\doc …`, `\quit`).
//! See `docs/specs/shell.md`.

use prism_client::QueryResult;
use prism_doc::{DocValue, Document};
use prism_protocol::Value;

/// A parsed shell command.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// A blank line.
    Empty,
    /// Show help.
    Help,
    /// Quit the shell.
    Quit,
    /// Round-trip a ping.
    Ping,
    /// Begin a transaction.
    Begin,
    /// Commit the current transaction.
    Commit,
    /// Abort the current transaction.
    Abort,
    /// A SQL statement.
    Sql(String),
    /// `\kv get <ns> <key>`.
    KvGet { ns: String, key: String },
    /// `\kv put <ns> <key> <value>`.
    KvPut {
        ns: String,
        key: String,
        value: String,
    },
    /// `\kv del <ns> <key>`.
    KvDel { ns: String, key: String },
    /// `\doc find <collection>` (matches all documents).
    DocFind { collection: String },
    /// `\doc insert <collection> <field>=<value> …`.
    DocInsert {
        collection: String,
        fields: Vec<(String, DocValue)>,
    },
}

/// Parse one input line into a [`Command`].
pub fn parse_command(line: &str) -> Result<Command, String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(Command::Empty);
    }
    if !trimmed.starts_with('\\') {
        return Ok(Command::Sql(trimmed.to_string()));
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    match parts[0] {
        "\\help" | "\\h" | "\\?" => Ok(Command::Help),
        "\\q" | "\\quit" | "\\exit" => Ok(Command::Quit),
        "\\ping" => Ok(Command::Ping),
        "\\begin" => Ok(Command::Begin),
        "\\commit" => Ok(Command::Commit),
        "\\abort" | "\\rollback" => Ok(Command::Abort),
        "\\kv" => parse_kv(&parts),
        "\\doc" => parse_doc(&parts),
        other => Err(format!("unknown command {other:?} (try \\help)")),
    }
}

fn parse_kv(parts: &[&str]) -> Result<Command, String> {
    match parts.get(1).copied() {
        Some("get") => {
            let (ns, key) = two_args(parts, "\\kv get <ns> <key>")?;
            Ok(Command::KvGet { ns, key })
        }
        Some("del") | Some("delete") => {
            let (ns, key) = two_args(parts, "\\kv del <ns> <key>")?;
            Ok(Command::KvDel { ns, key })
        }
        Some("put") => {
            if parts.len() < 5 {
                return Err("usage: \\kv put <ns> <key> <value>".into());
            }
            Ok(Command::KvPut {
                ns: parts[2].to_string(),
                key: parts[3].to_string(),
                value: parts[4..].join(" "),
            })
        }
        _ => Err("usage: \\kv <get|put|del> …".into()),
    }
}

fn parse_doc(parts: &[&str]) -> Result<Command, String> {
    match parts.get(1).copied() {
        Some("find") => {
            let collection = parts
                .get(2)
                .ok_or("usage: \\doc find <collection>")?
                .to_string();
            Ok(Command::DocFind { collection })
        }
        Some("insert") => {
            let collection = parts
                .get(2)
                .ok_or("usage: \\doc insert <collection> <field>=<value> …")?
                .to_string();
            let mut fields = Vec::new();
            for token in &parts[3..] {
                let (name, value) = token
                    .split_once('=')
                    .ok_or_else(|| format!("expected field=value, got {token:?}"))?;
                fields.push((name.to_string(), parse_value(value)));
            }
            Ok(Command::DocInsert { collection, fields })
        }
        _ => Err("usage: \\doc <find|insert> …".into()),
    }
}

fn two_args(parts: &[&str], usage: &str) -> Result<(String, String), String> {
    match (parts.get(2), parts.get(3)) {
        (Some(a), Some(b)) => Ok((a.to_string(), b.to_string())),
        _ => Err(format!("usage: {usage}")),
    }
}

/// Infer a [`DocValue`] from shell text: `true`/`false`/`null`, an integer, a
/// float, or otherwise a string (optionally quoted).
pub fn parse_value(s: &str) -> DocValue {
    match s {
        "true" => return DocValue::Bool(true),
        "false" => return DocValue::Bool(false),
        "null" => return DocValue::Null,
        _ => {}
    }
    if let Ok(n) = s.parse::<i64>() {
        return DocValue::Int64(n);
    }
    if let Ok(f) = s.parse::<f64>() {
        return DocValue::Double(f);
    }
    let unquoted = s
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(s);
    DocValue::Str(unquoted.to_string())
}

/// The help text.
pub fn help_text() -> &'static str {
    "\
commands:
  <sql>                         run a SQL statement
  \\ping                         round-trip a ping
  \\begin | \\commit | \\abort     transaction control
  \\kv get <ns> <key>            read a key
  \\kv put <ns> <key> <value>    write a key
  \\kv del <ns> <key>            delete a key
  \\doc find <collection>        list documents
  \\doc insert <coll> k=v …      insert a document
  \\help                         show this help
  \\quit                         exit"
}

/// Render a SQL result: an aligned table for SELECTs, or an affected-rows line.
pub fn render_query(result: &QueryResult) -> String {
    if result.columns.is_empty() {
        return format!("OK, {} row(s) affected", result.affected);
    }

    let headers: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    let cells: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|row| row.iter().map(render_cell).collect())
        .collect();

    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            cells
                .iter()
                .map(|row| row.get(i).map_or(0, |s| s.len()))
                .max()
                .unwrap_or(0)
                .max(h.len())
        })
        .collect();

    let mut out = String::new();
    push_row(&mut out, &headers, &widths);
    let separators: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    push_row(&mut out, &separators, &widths);
    for row in &cells {
        push_row(&mut out, row, &widths);
    }
    out.push_str(&format!("({} row(s))", result.rows.len()));
    out
}

fn push_row(out: &mut String, cells: &[String], widths: &[usize]) {
    let line: Vec<String> = cells
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{:width$}", c, width = widths.get(i).copied().unwrap_or(0)))
        .collect();
    out.push_str(&line.join(" | "));
    out.push('\n');
}

fn render_cell(cell: &Option<Value>) -> String {
    match cell {
        None => "NULL".to_string(),
        Some(v) => render_value(v),
    }
}

/// Render a wire [`Value`] for display.
pub fn render_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int32(n) => n.to_string(),
        Value::Int64(n) => n.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Str(s) => s.clone(),
        Value::Binary { bytes, .. } => format!("<{} bytes>", bytes.len()),
        Value::Timestamp(t) => t.to_string(),
        Value::ObjectId(id) => hex(id),
    }
}

/// Render a stored document (tagged-binary bytes) as `{ k: v, … }`.
pub fn render_document(bytes: &[u8]) -> String {
    let doc = match Document::decode(bytes) {
        Ok(d) => d,
        Err(_) => return "<corrupt document>".to_string(),
    };
    let fields: Vec<String> = doc
        .iter()
        .map(|(k, v)| format!("{k}: {}", render_doc_value(v)))
        .collect();
    format!("{{ {} }}", fields.join(", "))
}

fn render_doc_value(value: &DocValue) -> String {
    match value {
        DocValue::Null => "null".to_string(),
        DocValue::Bool(b) => b.to_string(),
        DocValue::Int32(n) => n.to_string(),
        DocValue::Int64(n) => n.to_string(),
        DocValue::Double(d) => d.to_string(),
        DocValue::Str(s) => format!("\"{s}\""),
        DocValue::Timestamp(t) => t.to_string(),
        DocValue::ObjectId(id) => id.to_hex(),
    }
}

/// Lowercase hex rendering of a byte slice (e.g. an inserted `_id`).
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_protocol::ColumnDesc;

    #[test]
    fn parses_sql_and_backslash_commands() {
        assert_eq!(
            parse_command("SELECT * FROM t").unwrap(),
            Command::Sql("SELECT * FROM t".into())
        );
        assert_eq!(parse_command("").unwrap(), Command::Empty);
        assert_eq!(parse_command("   ").unwrap(), Command::Empty);
        assert_eq!(parse_command("\\ping").unwrap(), Command::Ping);
        assert_eq!(parse_command("\\q").unwrap(), Command::Quit);
        assert_eq!(parse_command("\\begin").unwrap(), Command::Begin);
        assert!(parse_command("\\nope").is_err());
    }

    #[test]
    fn parses_kv_commands() {
        assert_eq!(
            parse_command("\\kv get sess abc").unwrap(),
            Command::KvGet {
                ns: "sess".into(),
                key: "abc".into()
            }
        );
        assert_eq!(
            parse_command("\\kv put sess abc hello world").unwrap(),
            Command::KvPut {
                ns: "sess".into(),
                key: "abc".into(),
                value: "hello world".into() // value keeps the remaining tokens
            }
        );
        assert!(parse_command("\\kv put onlyns").is_err());
    }

    #[test]
    fn parses_doc_insert_with_typed_fields() {
        let cmd = parse_command("\\doc insert users name=alice age=30 active=true").unwrap();
        assert_eq!(
            cmd,
            Command::DocInsert {
                collection: "users".into(),
                fields: vec![
                    ("name".into(), DocValue::Str("alice".into())),
                    ("age".into(), DocValue::Int64(30)),
                    ("active".into(), DocValue::Bool(true)),
                ],
            }
        );
    }

    #[test]
    fn infers_value_types() {
        assert_eq!(parse_value("42"), DocValue::Int64(42));
        assert_eq!(parse_value("3.5"), DocValue::Double(3.5));
        assert_eq!(parse_value("true"), DocValue::Bool(true));
        assert_eq!(parse_value("null"), DocValue::Null);
        assert_eq!(parse_value("hi"), DocValue::Str("hi".into()));
        assert_eq!(parse_value("\"quoted\""), DocValue::Str("quoted".into()));
    }

    #[test]
    fn renders_affected_and_tables() {
        let affected = QueryResult {
            columns: vec![],
            rows: vec![],
            affected: 3,
        };
        assert_eq!(render_query(&affected), "OK, 3 row(s) affected");

        let table = QueryResult {
            columns: vec![
                ColumnDesc {
                    name: "id".into(),
                    type_tag: 0x03,
                    nullable: false,
                },
                ColumnDesc {
                    name: "name".into(),
                    type_tag: 0x05,
                    nullable: true,
                },
            ],
            rows: vec![
                vec![Some(Value::Int64(1)), Some(Value::Str("alice".into()))],
                vec![Some(Value::Int64(2)), None],
            ],
            affected: 0,
        };
        let rendered = render_query(&table);
        assert!(rendered.contains("id"));
        assert!(rendered.contains("name"));
        assert!(rendered.contains("alice"));
        assert!(rendered.contains("NULL"));
        assert!(rendered.contains("(2 row(s))"));
    }

    #[test]
    fn renders_documents() {
        let doc = Document::from_fields([
            ("name".to_string(), DocValue::Str("bob".into())),
            ("age".to_string(), DocValue::Int64(25)),
        ]);
        let rendered = render_document(&doc.encode().unwrap());
        assert!(rendered.contains("name: \"bob\""));
        assert!(rendered.contains("age: 25"));
        assert_eq!(render_document(b"\xff\xff garbage"), "<corrupt document>");
    }
}
