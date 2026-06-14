"""The document query filter and its wire encoding.

Mirrors ``prism_protocol::DocQuery`` (crates/prism-protocol/src/data.rs): a tag
byte, then the operator-specific body. Operand values reuse the tagged
``Value`` encoding. Build queries with the :class:`Q` helpers."""

from __future__ import annotations

from typing import List, Sequence

from ._codec import Writer
from .errors import ProtocolError
from .value import Value, encode_tagged

# Discriminant tags, identical to the Rust ``DocQuery::encode``.
_Q_ALL = 0
_Q_EQ = 1
_Q_NE = 2
_Q_GT = 3
_Q_LT = 4
_Q_GTE = 5
_Q_LTE = 6
_Q_IN = 7
_Q_NIN = 8
_Q_EXISTS = 9
_Q_AND = 10
_Q_OR = 11
_Q_NOT = 12


class DocQuery:
    """A document query filter. Construct via the :class:`Q` helpers."""

    __slots__ = ("kind", "tag", "field", "value", "values", "present", "subs", "sub")

    def __init__(self, kind: str, **kw: object) -> None:
        self.kind = kind
        for k, v in kw.items():
            setattr(self, k, v)


class Q:
    """Query builders mirroring the engine's filter set."""

    @staticmethod
    def all() -> DocQuery:
        """Match every document."""
        return DocQuery("all")

    @staticmethod
    def eq(field: str, value: Value) -> DocQuery:
        return DocQuery("field", tag=_Q_EQ, field=field, value=value)

    @staticmethod
    def ne(field: str, value: Value) -> DocQuery:
        return DocQuery("field", tag=_Q_NE, field=field, value=value)

    @staticmethod
    def gt(field: str, value: Value) -> DocQuery:
        return DocQuery("field", tag=_Q_GT, field=field, value=value)

    @staticmethod
    def lt(field: str, value: Value) -> DocQuery:
        return DocQuery("field", tag=_Q_LT, field=field, value=value)

    @staticmethod
    def gte(field: str, value: Value) -> DocQuery:
        return DocQuery("field", tag=_Q_GTE, field=field, value=value)

    @staticmethod
    def lte(field: str, value: Value) -> DocQuery:
        return DocQuery("field", tag=_Q_LTE, field=field, value=value)

    @staticmethod
    def in_(field: str, values: Sequence[Value]) -> DocQuery:
        return DocQuery("set", tag=_Q_IN, field=field, values=list(values))

    @staticmethod
    def nin(field: str, values: Sequence[Value]) -> DocQuery:
        return DocQuery("set", tag=_Q_NIN, field=field, values=list(values))

    @staticmethod
    def exists(field: str, present: bool = True) -> DocQuery:
        return DocQuery("exists", field=field, present=present)

    @staticmethod
    def and_(*subs: DocQuery) -> DocQuery:
        return DocQuery("group", tag=_Q_AND, subs=list(subs))

    @staticmethod
    def or_(*subs: DocQuery) -> DocQuery:
        return DocQuery("group", tag=_Q_OR, subs=list(subs))

    @staticmethod
    def not_(sub: DocQuery) -> DocQuery:
        return DocQuery("not", sub=sub)


def _encode_query(w: Writer, q: DocQuery) -> None:
    if q.kind == "all":
        w.u8(_Q_ALL)
    elif q.kind == "field":
        w.u8(q.tag)
        w.str_u16(q.field)
        encode_tagged(w, q.value)
    elif q.kind == "set":
        w.u8(q.tag)
        w.str_u16(q.field)
        values: List[Value] = q.values
        if len(values) > 0xFFFFFFFF:
            raise ProtocolError("query set too large")
        w.u32(len(values))
        for v in values:
            encode_tagged(w, v)
    elif q.kind == "exists":
        w.u8(_Q_EXISTS)
        w.str_u16(q.field)
        w.u8(1 if q.present else 0)
    elif q.kind == "group":
        w.u8(q.tag)
        subs: List[DocQuery] = q.subs
        w.u32(len(subs))
        for s in subs:
            _encode_query(w, s)
    elif q.kind == "not":
        w.u8(_Q_NOT)
        _encode_query(w, q.sub)
    else:  # pragma: no cover - guarded by builders
        raise ProtocolError(f"unknown query kind {q.kind!r}")


def encode_doc_query(q: DocQuery) -> bytes:
    """Encode a query to the standalone bytes carried in a document command."""
    w = Writer()
    _encode_query(w, q)
    return w.out()
