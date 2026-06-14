"""Protocol messages: the 12-byte header plus per-message bodies.

Mirrors ``crates/prism-protocol/src/message.rs``. The SDK *encodes* the client
messages (as ``(type_code, body)`` pairs) and *decodes* the server messages;
both directions share the header and the error trailer."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import List, Optional, Tuple

from ._codec import Reader, Writer
from .errors import ErrorInfo, ProtocolError
from .value import ObjectId, Value, decode_untagged, encode_tagged


class MSG:
    """Message-type discriminants (first header byte)."""

    HELLO = 0x01
    HELLO_ACK = 0x02
    AUTH = 0x03
    AUTH_ACK = 0x04
    BEGIN = 0x10
    COMMIT = 0x11
    ABORT = 0x12
    TXN_ACK = 0x13
    SQL_EXECUTE = 0x20
    SQL_RESULT = 0x21
    DOC_OP = 0x30
    DOC_RESULT = 0x31
    KV_OP = 0x40
    KV_RESULT = 0x41
    CANCEL = 0x50
    NOTICE = 0x60
    PING = 0x70
    PONG = 0x71


AUTH_PASSWORD = 1
AUTH_MTLS = 2
TXN_READ_WRITE = 0
TXN_READ_ONLY = 1

# `Hello` feature bit: the body carries a connect-time database name.
FEATURE_CONNECT_DB = 1 << 0

_RESERVED3 = b"\x00\x00\x00"
_RESERVED4 = b"\x00\x00\x00\x00"


# ---- header --------------------------------------------------------------


def encode_packet(request_id: int, type_code: int, body: bytes) -> bytes:
    """Encode a client message into a payload (12-byte header + body)."""
    w = Writer()
    w.u8(type_code)
    w.raw(_RESERVED3)
    w.u32(request_id)
    w.raw(_RESERVED4)
    w.raw(body)
    return w.out()


# ---- outgoing (client -> server) bodies ----------------------------------


def hello_body(
    protocol_version: int,
    client_name: str,
    client_version: str,
    features: int,
    database: str,
) -> Tuple[int, bytes]:
    w = Writer()
    w.u32(protocol_version)
    w.str_u16(client_name)
    w.str_u16(client_version)
    w.u32(features)
    # The database field only travels under its feature bit (matches the Rust
    # codec), so a no-database Hello stays byte-compatible with v1.
    if features & FEATURE_CONNECT_DB:
        w.str_u16(database)
    return MSG.HELLO, w.out()


def auth_body(mechanism: int, username: str, password: str) -> Tuple[int, bytes]:
    w = Writer()
    w.u8(mechanism)
    w.str_u16(username)
    if mechanism == AUTH_PASSWORD:
        w.str_u16(password)
    return MSG.AUTH, w.out()


def begin_body(mode: int) -> Tuple[int, bytes]:
    w = Writer()
    w.u8(mode)
    return MSG.BEGIN, w.out()


def commit_body(idempotency_key: int) -> Tuple[int, bytes]:
    w = Writer()
    w.u128(idempotency_key)
    return MSG.COMMIT, w.out()


def abort_body() -> Tuple[int, bytes]:
    return MSG.ABORT, b""


def sql_body(sql: str, params: List[Value], options: int) -> Tuple[int, bytes]:
    w = Writer()
    w.str_u32(sql)
    w.u16(len(params))
    for p in params:
        encode_tagged(w, p)
    w.u32(options)
    return MSG.SQL_EXECUTE, w.out()


def doc_body(op: int, collection: str, blobs: List[bytes]) -> Tuple[int, bytes]:
    """Encode a DocOp. ``blobs`` are the op-specific, already-encoded byte
    blobs (a document, or query/update/options), written u32-length-prefixed.
    insertMany (op 2) is handled by the caller via :func:`doc_insert_many_body`."""
    w = Writer()
    w.u8(op)
    w.str_u16(collection)
    for b in blobs:
        w.bytes_u32(b)
    return MSG.DOC_OP, w.out()


def doc_insert_many_body(collection: str, docs: List[bytes]) -> Tuple[int, bytes]:
    w = Writer()
    w.u8(2)
    w.str_u16(collection)
    w.u32(len(docs))
    for d in docs:
        w.bytes_u32(d)
    return MSG.DOC_OP, w.out()


def kv_get_body(namespace: str, key: bytes) -> Tuple[int, bytes]:
    w = Writer()
    w.u8(1)
    w.str_u16(namespace)
    w.bytes_u16(key)
    return MSG.KV_OP, w.out()


def kv_put_body(namespace: str, key: bytes, value: bytes) -> Tuple[int, bytes]:
    w = Writer()
    w.u8(2)
    w.str_u16(namespace)
    w.bytes_u16(key)
    w.bytes_u32(value)
    return MSG.KV_OP, w.out()


def kv_delete_body(namespace: str, key: bytes) -> Tuple[int, bytes]:
    w = Writer()
    w.u8(3)
    w.str_u16(namespace)
    w.bytes_u16(key)
    return MSG.KV_OP, w.out()


def ping_body() -> Tuple[int, bytes]:
    return MSG.PING, b""


# ---- incoming (server -> client) -----------------------------------------


@dataclass
class ColumnDesc:
    name: str
    type_tag: int
    nullable: bool


@dataclass
class HelloAck:
    status: int
    server_version: str
    features: int
    session_id: int
    error: Optional[ErrorInfo] = None


@dataclass
class AuthAck:
    status: int
    user_oid: int
    error: Optional[ErrorInfo] = None


@dataclass
class TxnAck:
    status: int
    txn_id: int
    commit_lsn: int
    error: Optional[ErrorInfo] = None


@dataclass
class SqlResultMsg:
    status: int
    affected_rows: int
    columns: List[ColumnDesc]
    rows: List[List[Value]]
    more_frames: bool
    error: Optional[ErrorInfo] = None


@dataclass
class DocResultMsg:
    status: int
    affected: int
    inserted_ids: List[ObjectId]
    docs: List[bytes]
    more_frames: bool
    error: Optional[ErrorInfo] = None


@dataclass
class KvResultMsg:
    status: int
    op: int
    value: Optional[bytes] = None
    entries: List[Tuple[bytes, bytes]] = field(default_factory=list)
    more_frames: bool = False
    error: Optional[ErrorInfo] = None


@dataclass
class Notice:
    severity: int
    code: int
    message: str


@dataclass
class Pong:
    pass


@dataclass
class ServerPacket:
    request_id: int
    message: object


def decode_packet(payload: bytes) -> ServerPacket:
    """Decode a payload (header + body) into a server packet."""
    r = Reader(payload)
    type_code = r.u8()
    r.raw(3)
    request_id = r.u32()
    r.raw(4)
    message = _decode_body(type_code, r)
    r.expect_end()
    return ServerPacket(request_id, message)


def _decode_trailer(r: Reader, status: int) -> Optional[ErrorInfo]:
    if status == 0:
        return None
    return ErrorInfo(
        code=r.u32(),
        message=r.str_u16(),
        sqlstate=r.raw(5).decode("ascii"),
        detail=r.str_u16(),
        position=r.u32(),
    )


def _decode_body(type_code: int, r: Reader) -> object:
    if type_code == MSG.HELLO_ACK:
        status = r.u8()
        server_version = r.str_u16()
        features = r.u32()
        session_id = r.u128()
        return HelloAck(status, server_version, features, session_id, _decode_trailer(r, status))
    if type_code == MSG.AUTH_ACK:
        status = r.u8()
        user_oid = r.u64()
        return AuthAck(status, user_oid, _decode_trailer(r, status))
    if type_code == MSG.TXN_ACK:
        status = r.u8()
        txn_id = r.u64()
        commit_lsn = r.u64()
        return TxnAck(status, txn_id, commit_lsn, _decode_trailer(r, status))
    if type_code == MSG.SQL_RESULT:
        status = r.u8()
        affected_rows = r.u64()
        col_count = r.u16()
        columns = [ColumnDesc(r.str_u16(), r.u8(), r.u8() != 0) for _ in range(col_count)]
        row_count = r.u32()
        rows = _decode_rows(columns, row_count, r)
        more_frames = r.u8() != 0
        return SqlResultMsg(status, affected_rows, columns, rows, more_frames, _decode_trailer(r, status))
    if type_code == MSG.DOC_RESULT:
        status = r.u8()
        affected = r.u64()
        id_count = r.u32()
        inserted_ids = [ObjectId(r.raw(12)) for _ in range(id_count)]
        doc_count = r.u32()
        docs = [r.bytes_u32() for _ in range(doc_count)]
        more_frames = r.u8() != 0
        return DocResultMsg(status, affected, inserted_ids, docs, more_frames, _decode_trailer(r, status))
    if type_code == MSG.KV_RESULT:
        status = r.u8()
        op = r.u8()
        return _decode_kv_body(status, op, r)
    if type_code == MSG.NOTICE:
        return Notice(r.u8(), r.u32(), r.str_u16())
    if type_code == MSG.PONG:
        return Pong()
    raise ProtocolError(f"unexpected server message type 0x{type_code:x}")


def _decode_rows(columns: List[ColumnDesc], row_count: int, r: Reader) -> List[List[Value]]:
    nb = (len(columns) + 7) // 8
    rows: List[List[Value]] = []
    for _ in range(row_count):
        bitmap = r.raw(nb)
        row: List[Value] = []
        for c in range(len(columns)):
            is_null = (bitmap[c >> 3] & (1 << (c & 7))) != 0
            row.append(None if is_null else decode_untagged(r, columns[c].type_tag))
        rows.append(row)
    return rows


def _decode_entries(r: Reader) -> List[Tuple[bytes, bytes]]:
    count = r.u32()
    return [(r.bytes_u16(), r.bytes_u32()) for _ in range(count)]


def _decode_kv_body(status: int, op: int, r: Reader) -> KvResultMsg:
    if op == 1:  # get
        found = r.u8() != 0
        value = r.bytes_u32() if found else None
        return KvResultMsg(status, op, value=value, error=_decode_trailer(r, status))
    if op in (2, 3):  # put / delete
        return KvResultMsg(status, op, error=_decode_trailer(r, status))
    if op in (4, 5):  # range / scan
        entries = _decode_entries(r)
        more_frames = r.u8() != 0
        return KvResultMsg(status, op, entries=entries, more_frames=more_frames, error=_decode_trailer(r, status))
    raise ProtocolError(f"unknown kv result op 0x{op:x}")
