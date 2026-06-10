//! The per-connection session: the transaction state machine and the request
//! dispatcher that turns a [`Message`] into engine calls and a response.
//!
//! A session has at most one open transaction. A query with no open transaction
//! runs in its own implicit transaction (begin → run → commit); a `Begin` starts
//! an explicit transaction that subsequent requests run in until `Commit`/
//! `Abort`. This is the single point where the wire protocol meets the
//! cross-model engine.
//!
//! **Documented simplifications (this increment):**
//! - SQL parameters are not yet bound (`SqlExecute.params` must be empty; use
//!   literals). `TxnAck.commit_lsn` is reported as 0.
//! - Document queries use the structured [`DocQuery`] wire filter (eq/ne/gt/lt/
//!   gte/lte/in/nin/exists and and/or/not), mapped to the engine's `Filter`.
//!   Updates are still a flat document treated as an implicit `$set` of each
//!   field; wire update operators (`$inc`, `$unset`, …) are a follow-up.
//! - KV `range`/`scan` are unsupported on the hash namespace.
//!
//! A network session ([`Session::new_authenticating`]) must complete the
//! `Hello` → `Auth` handshake (scrypt-verified credentials) before any request
//! is served; the embedded [`Session::new`] is pre-authenticated and trusted.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use prism_core::TxnId;
use prism_core::txn::{Snapshot, TxnHandle, TxnMode};
use prism_doc::{DocCollection, DocValue, Document, Filter, Update};
use prism_kv::KvNamespace;
use prism_protocol::{
    ColumnDesc, DocCommand, DocQuery, KvCommand, KvResultBody, Message, NoticeSeverity,
    PROTOCOL_VERSION, Packet, Row, TxnMode as WireTxnMode, Value as WireValue,
};
use prism_sql::{Outcome, Value as SqlValue};
use prism_wal::Lsn;

use crate::auth::SYSTEM_OID;
use crate::database::Database;
use crate::error::{Result, ServerError};

// AuthAck status codes (docs/specs/wire-protocol.md).
const AUTH_BAD_CREDENTIALS: u8 = 1;
// HelloAck status: non-zero means the server will close the connection.
const HELLO_VERSION_MISMATCH: u8 = 1;

const SERVER_VERSION: &str = concat!("prism ", env!("CARGO_PKG_VERSION"));

/// The transaction state of a session.
enum SessionTxn {
    /// No open transaction; queries auto-commit.
    None,
    /// An explicit transaction is open, driven across requests via the detached
    /// transaction lifecycle (`begin_detached`/`resume`/`commit_txn`).
    Explicit {
        txn_id: TxnId,
        mode: TxnMode,
        snapshot: Snapshot,
        last_lsn: Lsn,
    },
}

/// Where a session is in the authentication handshake.
enum AuthState {
    /// Awaiting `Hello`.
    New,
    /// `Hello` accepted; awaiting `Auth`.
    Greeted,
    /// Authenticated as the given user OID; requests are served.
    Authenticated { _user_oid: u64 },
}

/// A client session over a shared [`Database`].
pub struct Session {
    db: Arc<Database>,
    auth: AuthState,
    txn: SessionTxn,
    /// Set when a fatal handshake condition (version mismatch, bad credentials,
    /// out-of-order message) requires the connection to close after the reply.
    closing: bool,
}

impl Session {
    /// A trusted, already-authenticated session for the embedded API (no network
    /// handshake). Use [`Session::new_authenticating`] for connections.
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            auth: AuthState::Authenticated {
                _user_oid: SYSTEM_OID,
            },
            txn: SessionTxn::None,
            closing: false,
        }
    }

    /// A network session that must complete the `Hello` → `Auth` handshake
    /// before it will serve any request.
    pub fn new_authenticating(db: Arc<Database>) -> Self {
        Self {
            db,
            auth: AuthState::New,
            txn: SessionTxn::None,
            closing: false,
        }
    }

    /// Whether an explicit transaction is currently open.
    pub fn in_transaction(&self) -> bool {
        matches!(self.txn, SessionTxn::Explicit { .. })
    }

    /// Whether the connection should be closed after the latest response (a
    /// failed handshake or a protocol violation).
    pub fn is_closing(&self) -> bool {
        self.closing
    }

    /// Handle one request packet, echoing the `request_id` on the response.
    pub fn handle_packet(&mut self, request: Packet) -> Packet {
        Packet::new(request.request_id, self.handle(request.message))
    }

    /// Handle one request message and produce the response message, enforcing
    /// the authentication handshake before any query is served.
    pub fn handle(&mut self, request: Message) -> Message {
        match self.auth {
            AuthState::New => self.greet(request),
            AuthState::Greeted => self.authenticate(request),
            AuthState::Authenticated { .. } => self.dispatch(request),
        }
    }

    /// `New` state: only `Hello` is accepted; anything else closes the
    /// connection.
    fn greet(&mut self, request: Message) -> Message {
        match request {
            Message::Hello {
                protocol_version, ..
            } => {
                if protocol_version != PROTOCOL_VERSION {
                    self.closing = true;
                    return Message::HelloAck {
                        status: HELLO_VERSION_MISMATCH,
                        server_version: SERVER_VERSION.to_string(),
                        features: 0,
                        session_id: 0,
                        error: Some(
                            ServerError::State(format!(
                                "unsupported protocol version {protocol_version}; \
                                 this server speaks {PROTOCOL_VERSION}"
                            ))
                            .to_error_info(),
                        ),
                    };
                }
                self.auth = AuthState::Greeted;
                Message::HelloAck {
                    status: 0,
                    server_version: SERVER_VERSION.to_string(),
                    features: 0,
                    session_id: new_session_id(),
                    error: None,
                }
            }
            _ => {
                self.closing = true;
                Message::Notice {
                    severity: NoticeSeverity::Error,
                    code: 0x0001,
                    message: "expected Hello as the first message".into(),
                }
            }
        }
    }

    /// `Greeted` state: only `Auth` is accepted; success authenticates the
    /// session, failure closes the connection.
    fn authenticate(&mut self, request: Message) -> Message {
        match request {
            Message::Auth {
                username, password, ..
            } => match self.db.verify_user(&username, &password) {
                Some(user_oid) => {
                    self.auth = AuthState::Authenticated {
                        _user_oid: user_oid,
                    };
                    Message::AuthAck {
                        status: 0,
                        user_oid,
                        error: None,
                    }
                }
                None => {
                    self.closing = true;
                    Message::AuthAck {
                        status: AUTH_BAD_CREDENTIALS,
                        user_oid: 0,
                        error: Some(
                            ServerError::State("authentication failed".into()).to_error_info(),
                        ),
                    }
                }
            },
            _ => {
                self.closing = true;
                Message::Notice {
                    severity: NoticeSeverity::Error,
                    code: 0x0001,
                    message: "expected Auth after Hello".into(),
                }
            }
        }
    }

    /// `Authenticated` state: serve a request.
    fn dispatch(&mut self, request: Message) -> Message {
        match request {
            Message::Ping => Message::Pong,
            Message::Begin { mode } => self.begin(mode),
            Message::Commit { idempotency_key } => self.commit(idempotency_key),
            Message::Abort => self.abort(),
            Message::SqlExecute {
                sql,
                params,
                options: _,
            } => self.run_sql(sql, params),
            Message::DocOp {
                collection,
                command,
            } => self.run_doc(collection, command),
            Message::KvOp { namespace, command } => self.run_kv(namespace, command),
            other => Message::Notice {
                severity: NoticeSeverity::Error,
                code: 0x0001,
                message: format!(
                    "unexpected message type {:?} for an authenticated session",
                    other.message_type()
                ),
            },
        }
    }

    // ---- transaction control -------------------------------------------------

    fn begin(&mut self, mode: WireTxnMode) -> Message {
        if let SessionTxn::Explicit { txn_id, .. } = self.txn {
            return txn_ack_err(
                txn_id,
                &ServerError::State("a transaction is already open".into()),
            );
        }
        let mode = core_mode(mode);
        let (txn_id, snapshot) = self.db.txns().begin_detached(mode);
        self.txn = SessionTxn::Explicit {
            txn_id,
            mode,
            snapshot,
            last_lsn: Lsn::ZERO,
        };
        Message::TxnAck {
            status: 0,
            txn_id,
            commit_lsn: 0,
            error: None,
        }
    }

    fn commit(&mut self, idempotency_key: u128) -> Message {
        let (txn_id, mode, last_lsn) = match std::mem::replace(&mut self.txn, SessionTxn::None) {
            SessionTxn::Explicit {
                txn_id,
                mode,
                last_lsn,
                ..
            } => (txn_id, mode, last_lsn),
            SessionTxn::None => {
                return txn_ack_err(0, &ServerError::State("no transaction to commit".into()));
            }
        };

        // Idempotency: a retried commit with a key already recorded returns the
        // original outcome and discards this transaction's (duplicate) writes.
        if idempotency_key != 0 {
            if let Some((orig_txn, commit_lsn)) = self.db.idempotency_lookup(idempotency_key) {
                let _ = self.db.txns().abort_txn(txn_id, mode, last_lsn);
                return Message::TxnAck {
                    status: 0,
                    txn_id: orig_txn,
                    commit_lsn,
                    error: None,
                };
            }
        }

        match self.db.txns().commit_txn(txn_id, mode, last_lsn) {
            Ok(()) => {
                if idempotency_key != 0 {
                    self.db.idempotency_record(idempotency_key, txn_id, 0);
                }
                Message::TxnAck {
                    status: 0,
                    txn_id,
                    commit_lsn: 0, // reported as 0 this increment
                    error: None,
                }
            }
            Err(e) => txn_ack_err(txn_id, &ServerError::from(e)),
        }
    }

    fn abort(&mut self) -> Message {
        match std::mem::replace(&mut self.txn, SessionTxn::None) {
            SessionTxn::Explicit {
                txn_id,
                mode,
                last_lsn,
                ..
            } => match self.db.txns().abort_txn(txn_id, mode, last_lsn) {
                Ok(()) => Message::TxnAck {
                    status: 0,
                    txn_id,
                    commit_lsn: 0,
                    error: None,
                },
                Err(e) => txn_ack_err(txn_id, &ServerError::from(e)),
            },
            SessionTxn::None => {
                txn_ack_err(0, &ServerError::State("no transaction to abort".into()))
            }
        }
    }

    /// Run `f` in the current transaction: the open explicit one (resumed), or a
    /// fresh implicit one that auto-commits. On error inside an explicit
    /// transaction the whole transaction is aborted (Postgres-style).
    fn in_txn<R>(&mut self, f: impl FnOnce(&TxnHandle) -> Result<R>) -> Result<R> {
        let txns = self.db.txns();
        // Snapshot the explicit-txn state into locals so we don't hold a borrow
        // of `self.txn` while we later mutate it.
        let explicit = if let SessionTxn::Explicit {
            txn_id,
            mode,
            snapshot,
            last_lsn,
        } = &self.txn
        {
            Some((*txn_id, *mode, snapshot.clone(), *last_lsn))
        } else {
            None
        };

        match explicit {
            Some((txn_id, mode, snapshot, last_lsn)) => {
                let handle = txns.resume(txn_id, mode, snapshot, last_lsn);
                let result = f(&handle);
                let new_last = handle.last_lsn();
                drop(handle); // transient: does not abort
                match result {
                    Ok(value) => {
                        if let SessionTxn::Explicit { last_lsn, .. } = &mut self.txn {
                            *last_lsn = new_last;
                        }
                        Ok(value)
                    }
                    Err(e) => {
                        // A failed statement rolls back the whole transaction.
                        let _ = txns.abort_txn(txn_id, mode, new_last);
                        self.txn = SessionTxn::None;
                        Err(e)
                    }
                }
            }
            None => {
                let handle = txns.begin(TxnMode::ReadWrite);
                match f(&handle) {
                    Ok(value) => {
                        handle.commit()?;
                        Ok(value)
                    }
                    Err(e) => {
                        let _ = handle.abort();
                        Err(e)
                    }
                }
            }
        }
    }

    // ---- SQL -----------------------------------------------------------------

    fn run_sql(&mut self, sql: String, params: Vec<WireValue>) -> Message {
        if !params.is_empty() {
            return sql_result_err(&ServerError::Unsupported(
                "SQL parameters are not yet supported; use literals".into(),
            ));
        }
        let db = self.db.clone();
        match self.in_txn(|txn| db.sql().execute(txn, &sql).map_err(ServerError::from)) {
            Ok(outcome) => {
                // DDL is effective immediately; persist new tables to the catalog
                // so the schema survives restart.
                if matches!(outcome, Outcome::CreateTable) {
                    if let Err(e) = db.persist_sql_tables() {
                        return sql_result_err(&e);
                    }
                }
                outcome_to_sql_result(outcome)
            }
            Err(e) => sql_result_err(&e),
        }
    }

    // ---- document ------------------------------------------------------------

    fn run_doc(&mut self, collection: String, command: DocCommand) -> Message {
        let coll = match self.db.collection(&collection) {
            Ok(c) => c,
            Err(e) => return doc_result_err(&e),
        };
        match self.in_txn(|txn| dispatch_doc(&coll, txn, command)) {
            Ok(out) => Message::DocResult {
                status: 0,
                affected: out.affected,
                inserted_ids: out.inserted_ids,
                docs: out.docs,
                more_frames: false,
                error: None,
            },
            Err(e) => doc_result_err(&e),
        }
    }

    // ---- KV ------------------------------------------------------------------

    fn run_kv(&mut self, namespace: String, command: KvCommand) -> Message {
        let shape = empty_kv_body(&command);
        let ns = match self.db.kv_namespace(&namespace) {
            Ok(n) => n,
            Err(e) => {
                return Message::KvResult {
                    status: 1,
                    body: shape,
                    error: Some(e.to_error_info()),
                };
            }
        };
        match self.in_txn(|txn| dispatch_kv(&ns, txn, command)) {
            Ok(body) => Message::KvResult {
                status: 0,
                body,
                error: None,
            },
            Err(e) => Message::KvResult {
                status: 1,
                body: shape,
                error: Some(e.to_error_info()),
            },
        }
    }
}

impl Drop for Session {
    /// A session that goes away with an explicit transaction still open aborts
    /// it — so a dropped connection never leaves a transaction holding locks.
    fn drop(&mut self) {
        if let SessionTxn::Explicit {
            txn_id,
            mode,
            last_lsn,
            ..
        } = &self.txn
        {
            let _ = self.db.txns().abort_txn(*txn_id, *mode, *last_lsn);
        }
    }
}

// ---- request dispatch helpers ------------------------------------------------

/// What an executed document op produced.
struct DocOutcome {
    affected: u64,
    inserted_ids: Vec<[u8; 12]>,
    docs: Vec<Vec<u8>>,
}

fn dispatch_doc(coll: &DocCollection, txn: &TxnHandle, command: DocCommand) -> Result<DocOutcome> {
    Ok(match command {
        DocCommand::InsertOne(bytes) => {
            let id = coll.insert_one(txn, Document::decode(&bytes)?)?;
            DocOutcome {
                affected: 1,
                inserted_ids: id_bytes(&id).into_iter().collect(),
                docs: vec![],
            }
        }
        DocCommand::InsertMany(list) => {
            let docs = list
                .iter()
                .map(|b| Document::decode(b))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let ids = coll.insert_many(txn, docs)?;
            DocOutcome {
                affected: ids.len() as u64,
                inserted_ids: ids.iter().filter_map(id_bytes).collect(),
                docs: vec![],
            }
        }
        DocCommand::Find { query, .. } => {
            let found = coll.find(txn, &query_to_filter(&query)?)?;
            DocOutcome {
                affected: found.len() as u64,
                inserted_ids: vec![],
                docs: encode_docs(&found)?,
            }
        }
        DocCommand::FindOne { query, .. } => {
            let found = coll.find_one(txn, &query_to_filter(&query)?)?;
            let docs = match found {
                Some(d) => vec![d.encode()?],
                None => vec![],
            };
            DocOutcome {
                affected: docs.len() as u64,
                inserted_ids: vec![],
                docs,
            }
        }
        DocCommand::UpdateOne { query, update, .. } => {
            let n = coll.update_one(txn, &query_to_filter(&query)?, &update_to_update(&update)?)?;
            count_outcome(n)
        }
        DocCommand::UpdateMany { query, update, .. } => {
            let n =
                coll.update_many(txn, &query_to_filter(&query)?, &update_to_update(&update)?)?;
            count_outcome(n)
        }
        DocCommand::DeleteOne { query, .. } => {
            count_outcome(coll.delete_one(txn, &query_to_filter(&query)?)?)
        }
        DocCommand::DeleteMany { query, .. } => {
            count_outcome(coll.delete_many(txn, &query_to_filter(&query)?)?)
        }
    })
}

fn count_outcome(n: u64) -> DocOutcome {
    DocOutcome {
        affected: n,
        inserted_ids: vec![],
        docs: vec![],
    }
}

fn dispatch_kv(ns: &KvNamespace, txn: &TxnHandle, command: KvCommand) -> Result<KvResultBody> {
    Ok(match command {
        KvCommand::Get { key } => KvResultBody::Get {
            value: ns.get(txn, &key)?,
        },
        KvCommand::Put { key, value } => {
            ns.put(txn, &key, &value)?;
            KvResultBody::Put
        }
        KvCommand::Delete { key } => {
            ns.delete(txn, &key)?;
            KvResultBody::Delete
        }
        KvCommand::Range { .. } | KvCommand::Scan { .. } => {
            return Err(ServerError::Unsupported(
                "range/scan is not supported on a hash namespace".into(),
            ));
        }
    })
}

/// A default result body matching `command`'s op type, for error responses
/// (which still echo the op type).
fn empty_kv_body(command: &KvCommand) -> KvResultBody {
    match command {
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
    }
}

// ---- value / outcome mapping -------------------------------------------------

fn outcome_to_sql_result(outcome: Outcome) -> Message {
    let (affected_rows, columns, rows) = match outcome {
        Outcome::CreateTable => (0, vec![], vec![]),
        Outcome::Insert { count } => (count as u64, vec![], vec![]),
        Outcome::Select { columns, rows } => {
            let wire_rows: Vec<Row> = rows
                .iter()
                .map(|r| r.iter().map(sql_cell).collect())
                .collect();
            let descs = columns
                .iter()
                .enumerate()
                .map(|(i, name)| ColumnDesc {
                    name: name.clone(),
                    type_tag: infer_column_tag(&wire_rows, i),
                    nullable: true,
                })
                .collect();
            (0, descs, wire_rows)
        }
        Outcome::Update { count } | Outcome::Delete { count } => (count as u64, vec![], vec![]),
    };
    Message::SqlResult {
        status: 0,
        affected_rows,
        columns,
        rows,
        more_frames: false,
        error: None,
    }
}

fn sql_cell(v: &SqlValue) -> Option<WireValue> {
    match v {
        SqlValue::Null => None,
        SqlValue::Bool(b) => Some(WireValue::Bool(*b)),
        SqlValue::Int64(n) => Some(WireValue::Int64(*n)),
        SqlValue::Text(s) => Some(WireValue::Str(s.clone())),
    }
}

/// The column's wire type tag, taken from the first non-null cell (all non-null
/// cells in a SQL column share a type); 0x00 if the column is entirely null.
fn infer_column_tag(rows: &[Row], col: usize) -> u8 {
    rows.iter()
        .find_map(|r| r[col].as_ref().map(WireValue::type_tag))
        .unwrap_or(0x00)
}

fn id_bytes(id: &DocValue) -> Option<[u8; 12]> {
    match id {
        DocValue::ObjectId(oid) => Some(oid.0),
        _ => None,
    }
}

fn encode_docs(docs: &[Document]) -> Result<Vec<Vec<u8>>> {
    docs.iter()
        .map(|d| d.encode().map_err(ServerError::from))
        .collect()
}

/// Decode a wire [`DocQuery`] and map it to the engine's [`Filter`].
fn query_to_filter(bytes: &[u8]) -> Result<Filter> {
    doc_query_to_filter(DocQuery::from_bytes(bytes)?)
}

/// Map a wire [`DocQuery`] onto the document engine's [`Filter`], translating
/// each operand from the protocol's [`WireValue`] to a [`DocValue`].
fn doc_query_to_filter(q: DocQuery) -> Result<Filter> {
    Ok(match q {
        DocQuery::All => Filter::All,
        DocQuery::Eq(f, v) => Filter::Eq(f, wire_to_doc_value(v)?),
        DocQuery::Ne(f, v) => Filter::Ne(f, wire_to_doc_value(v)?),
        DocQuery::Gt(f, v) => Filter::Gt(f, wire_to_doc_value(v)?),
        DocQuery::Lt(f, v) => Filter::Lt(f, wire_to_doc_value(v)?),
        DocQuery::Gte(f, v) => Filter::Gte(f, wire_to_doc_value(v)?),
        DocQuery::Lte(f, v) => Filter::Lte(f, wire_to_doc_value(v)?),
        DocQuery::In(f, set) => Filter::In(f, wire_to_doc_values(set)?),
        DocQuery::Nin(f, set) => Filter::Nin(f, wire_to_doc_values(set)?),
        DocQuery::Exists(f, want) => Filter::Exists(f, want),
        DocQuery::And(subs) => Filter::And(map_subqueries(subs)?),
        DocQuery::Or(subs) => Filter::Or(map_subqueries(subs)?),
        DocQuery::Not(inner) => Filter::Not(Box::new(doc_query_to_filter(*inner)?)),
    })
}

fn map_subqueries(subs: Vec<DocQuery>) -> Result<Vec<Filter>> {
    subs.into_iter().map(doc_query_to_filter).collect()
}

fn wire_to_doc_values(values: Vec<WireValue>) -> Result<Vec<DocValue>> {
    values.into_iter().map(wire_to_doc_value).collect()
}

/// Translate a protocol scalar to a document scalar. Binary has no document
/// counterpart in this slice.
fn wire_to_doc_value(v: WireValue) -> Result<DocValue> {
    Ok(match v {
        WireValue::Null => DocValue::Null,
        WireValue::Bool(b) => DocValue::Bool(b),
        WireValue::Int32(n) => DocValue::Int32(n),
        WireValue::Int64(n) => DocValue::Int64(n),
        WireValue::Double(d) => DocValue::Double(d),
        WireValue::Str(s) => DocValue::Str(s),
        WireValue::Timestamp(t) => DocValue::Timestamp(t),
        WireValue::ObjectId(id) => DocValue::ObjectId(prism_doc::ObjectId(id)),
        WireValue::Binary { .. } => {
            return Err(ServerError::Unsupported(
                "binary values are not supported in document queries".into(),
            ));
        }
    })
}

/// Compile a flat update document into an implicit `$set` of each field.
fn update_to_update(bytes: &[u8]) -> Result<Update> {
    let doc = Document::decode(bytes)?;
    let mut update = Update::new();
    for (k, v) in doc.iter() {
        update = update.set(k.to_string(), v.clone());
    }
    Ok(update)
}

// ---- response builders for errors --------------------------------------------

fn sql_result_err(e: &ServerError) -> Message {
    Message::SqlResult {
        status: 1,
        affected_rows: 0,
        columns: vec![],
        rows: vec![],
        more_frames: false,
        error: Some(e.to_error_info()),
    }
}

fn doc_result_err(e: &ServerError) -> Message {
    Message::DocResult {
        status: 1,
        affected: 0,
        inserted_ids: vec![],
        docs: vec![],
        more_frames: false,
        error: Some(e.to_error_info()),
    }
}

fn txn_ack_err(txn_id: TxnId, e: &ServerError) -> Message {
    Message::TxnAck {
        status: 1,
        txn_id,
        commit_lsn: 0,
        error: Some(e.to_error_info()),
    }
}

fn core_mode(mode: WireTxnMode) -> TxnMode {
    match mode {
        WireTxnMode::ReadWrite => TxnMode::ReadWrite,
        WireTxnMode::ReadOnly => TxnMode::ReadOnly,
    }
}

fn new_session_id() -> u128 {
    // A best-effort unique id (time-based); replaced by a CSPRNG when auth lands.
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
