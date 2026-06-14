"""The high-level client: connect + handshake, then SQL / KV / document calls
and transaction control. One client owns one connection = one server session,
so a ``begin()`` … ``commit()`` brackets the calls in between."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Dict, List, Optional, Sequence, Union

from .connection import Connection, NoticeHandler, TlsArg
from .document import Document, decode_document, encode_document
from .errors import ErrorInfo, PrismServerError, ProtocolError
from .messages import (
    AUTH_PASSWORD,
    FEATURE_CONNECT_DB,
    TXN_READ_ONLY,
    TXN_READ_WRITE,
    AuthAck,
    ColumnDesc,
    DocResultMsg,
    HelloAck,
    KvResultMsg,
    Pong,
    SqlResultMsg,
    TxnAck,
    abort_body,
    auth_body,
    begin_body,
    commit_body,
    doc_body,
    doc_insert_many_body,
    hello_body,
    kv_delete_body,
    kv_get_body,
    kv_put_body,
    ping_body,
    sql_body,
)
from .query import DocQuery, Q, encode_doc_query
from .update import DocUpdate, encode_doc_update
from .value import ObjectId, Value

_PROTOCOL_VERSION = 1
_EMPTY = b""

BytesLike = Union[str, bytes, bytearray]


@dataclass
class SqlResult:
    """A SQL result set. ``rows`` are keyed by column name; ``raw`` keeps cell order."""

    columns: List[ColumnDesc]
    rows: List[Dict[str, Value]]
    raw: List[List[Value]]
    affected_rows: int


def _fail(error: Optional[ErrorInfo]) -> "None":
    raise PrismServerError(
        error or ErrorInfo(code=0, message="server error", sqlstate="XX000")
    )


def _bytes(v: BytesLike) -> bytes:
    return v.encode("utf-8") if isinstance(v, str) else bytes(v)


class KvSurface:
    """``client.kv`` — namespaced key/value operations."""

    def __init__(self, client: "Client") -> None:
        self._c = client

    def get(self, namespace: str, key: BytesLike) -> Optional[bytes]:
        reply = self._c._kv_reply(*kv_get_body(namespace, _bytes(key)))
        if reply.op != 1:
            raise ProtocolError("expected a KV get result")
        return reply.value

    def put(self, namespace: str, key: BytesLike, value: BytesLike) -> None:
        self._c._kv_reply(*kv_put_body(namespace, _bytes(key), _bytes(value)))

    def delete(self, namespace: str, key: BytesLike) -> None:
        self._c._kv_reply(*kv_delete_body(namespace, _bytes(key)))


class DocSurface:
    """``client.doc`` — document collection operations."""

    def __init__(self, client: "Client") -> None:
        self._c = client

    def insert_one(self, collection: str, document: Document) -> ObjectId:
        reply = self._c._doc_reply(*doc_body(1, collection, [encode_document(document)]))
        if not reply.inserted_ids:
            raise ProtocolError("insert returned no _id")
        return reply.inserted_ids[0]

    def insert_many(self, collection: str, documents: Sequence[Document]) -> List[ObjectId]:
        blobs = [encode_document(d) for d in documents]
        reply = self._c._doc_reply(*doc_insert_many_body(collection, blobs))
        return reply.inserted_ids

    def find(self, collection: str, query: Optional[DocQuery] = None) -> List[Document]:
        reply = self._c._doc_reply(*doc_body(3, collection, [encode_doc_query(query or Q.all()), _EMPTY]))
        return [decode_document(d) for d in reply.docs]

    def find_one(self, collection: str, query: Optional[DocQuery] = None) -> Optional[Document]:
        reply = self._c._doc_reply(*doc_body(4, collection, [encode_doc_query(query or Q.all()), _EMPTY]))
        return decode_document(reply.docs[0]) if reply.docs else None

    def count(self, collection: str, query: Optional[DocQuery] = None) -> int:
        reply = self._c._doc_reply(*doc_body(9, collection, [encode_doc_query(query or Q.all()), _EMPTY]))
        return reply.affected

    def update_one(self, collection: str, query: DocQuery, update: List[DocUpdate]) -> int:
        reply = self._c._doc_reply(
            *doc_body(5, collection, [encode_doc_query(query), encode_doc_update(update), _EMPTY])
        )
        return reply.affected

    def update_many(self, collection: str, query: DocQuery, update: List[DocUpdate]) -> int:
        reply = self._c._doc_reply(
            *doc_body(6, collection, [encode_doc_query(query), encode_doc_update(update), _EMPTY])
        )
        return reply.affected

    def delete_one(self, collection: str, query: DocQuery) -> int:
        reply = self._c._doc_reply(*doc_body(7, collection, [encode_doc_query(query), _EMPTY]))
        return reply.affected

    def delete_many(self, collection: str, query: DocQuery) -> int:
        reply = self._c._doc_reply(*doc_body(8, collection, [encode_doc_query(query), _EMPTY]))
        return reply.affected


class Client:
    """A connected, authenticated Prism session."""

    def __init__(self, conn: Connection) -> None:
        self._conn = conn
        self.kv = KvSurface(self)
        self.doc = DocSurface(self)

    @classmethod
    def connect(
        cls,
        host: str = "127.0.0.1",
        port: int = 4444,
        *,
        username: Optional[str] = None,
        password: Optional[str] = None,
        database: Optional[str] = None,
        tls: TlsArg = None,
        server_hostname: Optional[str] = None,
        connect_timeout: float = 10.0,
        client_name: str = "prismdb-python",
        client_version: str = "0.1.0",
        on_notice: Optional[NoticeHandler] = None,
    ) -> "Client":
        """Connect, perform the handshake, and (if ``username`` is set) authenticate."""
        conn = Connection.connect(
            host,
            port,
            tls=tls,
            server_hostname=server_hostname,
            connect_timeout=connect_timeout,
            on_notice=on_notice,
        )
        client = cls(conn)
        try:
            connect_db_honored = client._handshake(
                username, password, database or "", client_name, client_version
            )
            # Fall back to `USE` only when the server did not bind the database
            # in the handshake (an older server without FEATURE_CONNECT_DB).
            if database and not connect_db_honored:
                client.sql(f"USE {database}", return_rows=False)
        except Exception:
            conn.close()
            raise
        return client

    def _handshake(
        self,
        username: Optional[str],
        password: Optional[str],
        database: str,
        client_name: str,
        client_version: str,
    ) -> bool:
        features = FEATURE_CONNECT_DB if database else 0
        ack = self._conn.request(*hello_body(_PROTOCOL_VERSION, client_name, client_version, features, database))
        if not isinstance(ack, HelloAck):
            raise ProtocolError("expected HelloAck")
        if ack.status != 0:
            _fail(ack.error)
        connect_db_honored = (ack.features & FEATURE_CONNECT_DB) != 0 and database != ""

        if username is not None:
            auth_ack = self._conn.request(*auth_body(AUTH_PASSWORD, username, password or ""))
            if not isinstance(auth_ack, AuthAck):
                raise ProtocolError("expected AuthAck")
            if auth_ack.status != 0:
                _fail(auth_ack.error)
        return connect_db_honored

    # ---- SQL --------------------------------------------------------------

    def sql(
        self,
        text: str,
        params: Optional[Sequence[Value]] = None,
        *,
        return_rows: bool = True,
    ) -> SqlResult:
        """Execute a SQL statement. Returns rows for ``SELECT``, counts otherwise."""
        reply = self._conn.request(*sql_body(text, list(params or []), 1 if return_rows else 0))
        if not isinstance(reply, SqlResultMsg):
            raise ProtocolError("expected SqlResult")
        if reply.status != 0:
            _fail(reply.error)
        if reply.more_frames:
            raise ProtocolError("streamed SQL results are not yet supported")
        names = [c.name for c in reply.columns]
        rows = [{names[i]: cell for i, cell in enumerate(cells)} for cells in reply.rows]
        return SqlResult(reply.columns, rows, reply.rows, reply.affected_rows)

    # ---- transactions -----------------------------------------------------

    def begin(self, mode: str = "read_write") -> int:
        """Begin a transaction; returns the assigned transaction id."""
        ack = self._txn(*begin_body(TXN_READ_ONLY if mode == "read_only" else TXN_READ_WRITE))
        return ack.txn_id

    def commit(self, idempotency_key: int = 0) -> None:
        """Commit the current transaction (optionally idempotent)."""
        self._txn(*commit_body(idempotency_key))

    def abort(self) -> None:
        """Abort the current transaction."""
        self._txn(*abort_body())

    def _txn(self, type_code: int, body: bytes) -> TxnAck:
        reply = self._conn.request(type_code, body)
        if not isinstance(reply, TxnAck):
            raise ProtocolError("expected TxnAck")
        if reply.status != 0:
            _fail(reply.error)
        return reply

    # ---- misc -------------------------------------------------------------

    def ping(self) -> None:
        """Round-trip a keep-alive ping."""
        reply = self._conn.request(*ping_body())
        if not isinstance(reply, Pong):
            raise ProtocolError("expected Pong")

    def close(self) -> None:
        """Close the underlying connection."""
        self._conn.close()

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    # ---- internal reply helpers -------------------------------------------

    def _kv_reply(self, type_code: int, body: bytes) -> KvResultMsg:
        reply = self._conn.request(type_code, body)
        if not isinstance(reply, KvResultMsg):
            raise ProtocolError("expected KvResult")
        if reply.status != 0:
            _fail(reply.error)
        return reply

    def _doc_reply(self, type_code: int, body: bytes) -> DocResultMsg:
        reply = self._conn.request(type_code, body)
        if not isinstance(reply, DocResultMsg):
            raise ProtocolError("expected DocResult")
        if reply.status != 0:
            _fail(reply.error)
        if reply.more_frames:
            raise ProtocolError("streamed document results are not yet supported")
        return reply
