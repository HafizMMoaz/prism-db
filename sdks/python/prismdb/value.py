"""The scalar value model and its tagged/untagged wire codec.

Mirrors ``crates/prism-protocol/src/data.rs`` (``Value``) and the type tags in
``docs/specs/record-format.md``. The SDK accepts plain Python values and maps
them to wire types; on decode it returns plain Python values."""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Union

from ._codec import Reader, Writer
from .errors import ProtocolError


class TAG:
    """Record-format type tags."""

    NULL = 0x00
    BOOL = 0x01
    INT32 = 0x02
    INT64 = 0x03
    DOUBLE = 0x04
    STRING = 0x05
    BINARY = 0x06
    TIMESTAMP = 0x09
    OBJECTID = 0x0A


class ObjectId:
    """A 12-byte document identifier."""

    __slots__ = ("bytes",)

    def __init__(self, raw: bytes) -> None:
        if len(raw) != 12:
            raise ProtocolError("ObjectId must be 12 bytes")
        self.bytes = bytes(raw)

    def to_hex(self) -> str:
        """Lowercase 24-character hex."""
        return self.bytes.hex()

    @classmethod
    def from_hex(cls, hex_str: str) -> "ObjectId":
        if len(hex_str) != 24:
            raise ProtocolError("ObjectId hex must be 24 chars")
        return cls(bytes.fromhex(hex_str))

    def __str__(self) -> str:
        return self.to_hex()

    def __repr__(self) -> str:
        return f"ObjectId('{self.to_hex()}')"

    def __eq__(self, other: object) -> bool:
        return isinstance(other, ObjectId) and other.bytes == self.bytes

    def __hash__(self) -> int:
        return hash(self.bytes)


class Typed:
    """An explicitly-typed value, for cases where the default mapping of a
    Python value is not what you want (e.g. a 32-bit int, a float that happens
    to be integral, or a timestamp). Build with :func:`int32` / :func:`int64` /
    :func:`float64` / :func:`timestamp`."""

    __slots__ = ("tag", "value")

    def __init__(self, tag: int, value: Union[int, float]) -> None:
        self.tag = tag
        self.value = value


def int32(n: int) -> Typed:
    """Force a value to wire ``Int32``."""
    return Typed(TAG.INT32, int(n))


def int64(n: int) -> Typed:
    """Force a value to wire ``Int64``."""
    return Typed(TAG.INT64, int(n))


def float64(n: float) -> Typed:
    """Force a value to wire ``Double``."""
    return Typed(TAG.DOUBLE, float(n))


def timestamp(us: int) -> Typed:
    """Force a value to wire ``Timestamp`` (microseconds since the Unix epoch)."""
    return Typed(TAG.TIMESTAMP, int(us))


# A value the SDK can send. Plain Python values map as: ``None``->Null,
# ``bool``->Bool, ``int``->Int64, ``float``->Double, ``str``->Str,
# ``bytes``->Binary, ``datetime``->Timestamp, ``ObjectId``->ObjectId. Wrap with
# int32/float64/timestamp for other types.
Value = Union[
    None, bool, int, float, str, bytes, bytearray, datetime, ObjectId, Typed
]


def tag_of(v: Value) -> int:
    """Resolve a Python value to its wire type tag."""
    if v is None:
        return TAG.NULL
    if isinstance(v, Typed):
        return v.tag
    if isinstance(v, ObjectId):
        return TAG.OBJECTID
    if isinstance(v, bool):  # must precede int - bool is a subclass of int
        return TAG.BOOL
    if isinstance(v, int):
        return TAG.INT64
    if isinstance(v, float):
        return TAG.DOUBLE
    if isinstance(v, str):
        return TAG.STRING
    if isinstance(v, datetime):
        return TAG.TIMESTAMP
    if isinstance(v, (bytes, bytearray)):
        return TAG.BINARY
    raise ProtocolError(f"unsupported value: {v!r}")


def _epoch_micros(dt: datetime) -> int:
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return int(dt.timestamp() * 1_000_000)


def encode_untagged(w: Writer, tag: int, v: Value) -> None:
    """Write the value bytes for a known ``tag`` (no tag byte)."""
    raw = v.value if isinstance(v, Typed) else v
    if tag == TAG.NULL:
        return
    if tag == TAG.BOOL:
        w.u8(1 if raw else 0)
    elif tag == TAG.INT32:
        w.i32(int(raw))
    elif tag == TAG.INT64:
        w.i64(int(raw))
    elif tag == TAG.DOUBLE:
        w.f64(float(raw))
    elif tag == TAG.TIMESTAMP:
        w.i64(_epoch_micros(raw) if isinstance(raw, datetime) else int(raw))
    elif tag == TAG.STRING:
        w.str_u32(str(raw))
    elif tag == TAG.OBJECTID:
        w.raw(raw.bytes)  # type: ignore[union-attr]
    elif tag == TAG.BINARY:
        b = bytes(raw)  # type: ignore[arg-type]
        w.u32(len(b))
        w.u8(0)  # subtype
        w.raw(b)
    else:
        raise ProtocolError(f"cannot encode value tag 0x{tag:x}")


def encode_tagged(w: Writer, v: Value) -> None:
    """Write a tagged value (tag byte then the value bytes)."""
    tag = tag_of(v)
    w.u8(tag)
    encode_untagged(w, tag, v)


def decode_untagged(r: Reader, tag: int) -> Value:
    """Read the value bytes for a known ``tag``, returning a plain Python value."""
    if tag == TAG.NULL:
        return None
    if tag == TAG.BOOL:
        return r.u8() != 0
    if tag == TAG.INT32:
        return r.i32()
    if tag == TAG.INT64:
        return r.i64()
    if tag == TAG.DOUBLE:
        return r.f64()
    if tag == TAG.TIMESTAMP:
        return r.i64()
    if tag == TAG.STRING:
        return r.str_u32()
    if tag == TAG.OBJECTID:
        return ObjectId(r.raw(12))
    if tag == TAG.BINARY:
        n = r.u32()
        r.u8()  # subtype (discarded)
        return r.raw(n)
    raise ProtocolError(f"unknown value tag 0x{tag:x}")


def decode_tagged(r: Reader) -> Value:
    """Read a tagged value."""
    return decode_untagged(r, r.u8())
