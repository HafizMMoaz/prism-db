"""Low-level binary codec: a growable little-endian ``Writer``, a bounds-checked
``Reader``, and the length-prefixed frame helpers. The byte layouts mirror
``crates/prism-protocol/src/codec.rs`` exactly (all multi-byte integers LE)."""

from __future__ import annotations

import struct

from .errors import ProtocolError

_U64_MASK = (1 << 64) - 1
_U128_MASK = (1 << 128) - 1


class Writer:
    """A growable little-endian writer over a bytearray."""

    __slots__ = ("_buf",)

    def __init__(self) -> None:
        self._buf = bytearray()

    def u8(self, v: int) -> None:
        self._buf.append(v & 0xFF)

    def u16(self, v: int) -> None:
        self._buf += struct.pack("<H", v & 0xFFFF)

    def u32(self, v: int) -> None:
        self._buf += struct.pack("<I", v & 0xFFFFFFFF)

    def i32(self, v: int) -> None:
        self._buf += struct.pack("<i", _to_signed(v, 32))

    def u64(self, v: int) -> None:
        self._buf += struct.pack("<Q", v & _U64_MASK)

    def i64(self, v: int) -> None:
        self._buf += struct.pack("<q", _to_signed(v, 64))

    def f64(self, v: float) -> None:
        self._buf += struct.pack("<d", v)

    def u128(self, v: int) -> None:
        """A 128-bit unsigned integer as 16 little-endian bytes."""
        x = v & _U128_MASK
        self._buf += struct.pack("<QQ", x & _U64_MASK, (x >> 64) & _U64_MASK)

    def raw(self, b: bytes) -> None:
        self._buf += b

    def str_u16(self, s: str) -> None:
        """A UTF-8 string with a u16 length prefix."""
        b = s.encode("utf-8")
        self.u16(len(b))
        self._buf += b

    def str_u32(self, s: str) -> None:
        """A UTF-8 string with a u32 length prefix."""
        b = s.encode("utf-8")
        self.u32(len(b))
        self._buf += b

    def bytes_u16(self, b: bytes) -> None:
        """A byte string with a u16 length prefix."""
        self.u16(len(b))
        self._buf += b

    def bytes_u32(self, b: bytes) -> None:
        """A byte string with a u32 length prefix."""
        self.u32(len(b))
        self._buf += b

    def out(self) -> bytes:
        return bytes(self._buf)


class Reader:
    """A bounds-checked little-endian reader over a bytes-like object."""

    __slots__ = ("_buf", "_p")

    def __init__(self, buf: bytes) -> None:
        self._buf = buf
        self._p = 0

    def _need(self, n: int) -> None:
        if self._p + n > len(self._buf):
            raise ProtocolError(f"truncated: need {n} bytes at offset {self._p}")

    def u8(self) -> int:
        self._need(1)
        v = self._buf[self._p]
        self._p += 1
        return v

    def u16(self) -> int:
        self._need(2)
        v = struct.unpack_from("<H", self._buf, self._p)[0]
        self._p += 2
        return v

    def u32(self) -> int:
        self._need(4)
        v = struct.unpack_from("<I", self._buf, self._p)[0]
        self._p += 4
        return v

    def i32(self) -> int:
        self._need(4)
        v = struct.unpack_from("<i", self._buf, self._p)[0]
        self._p += 4
        return v

    def u64(self) -> int:
        self._need(8)
        v = struct.unpack_from("<Q", self._buf, self._p)[0]
        self._p += 8
        return v

    def i64(self) -> int:
        self._need(8)
        v = struct.unpack_from("<q", self._buf, self._p)[0]
        self._p += 8
        return v

    def f64(self) -> float:
        self._need(8)
        v = struct.unpack_from("<d", self._buf, self._p)[0]
        self._p += 8
        return v

    def u128(self) -> int:
        self._need(16)
        lo, hi = struct.unpack_from("<QQ", self._buf, self._p)
        self._p += 16
        return (hi << 64) | lo

    def raw(self, n: int) -> bytes:
        self._need(n)
        s = self._buf[self._p : self._p + n]
        self._p += n
        return bytes(s)

    def str_u16(self) -> str:
        return self.raw(self.u16()).decode("utf-8")

    def str_u32(self) -> str:
        return self.raw(self.u32()).decode("utf-8")

    def bytes_u16(self) -> bytes:
        return self.raw(self.u16())

    def bytes_u32(self) -> bytes:
        return self.raw(self.u32())

    def remaining(self) -> int:
        return len(self._buf) - self._p

    def expect_end(self) -> None:
        """Raise unless every byte has been consumed."""
        if self.remaining() != 0:
            raise ProtocolError(f"{self.remaining()} trailing byte(s) after message")


def _to_signed(v: int, bits: int) -> int:
    """Wrap ``v`` into a signed ``bits``-wide integer (two's complement)."""
    mask = (1 << bits) - 1
    v &= mask
    if v >= 1 << (bits - 1):
        v -= 1 << bits
    return v


def frame_encode(payload: bytes) -> bytes:
    """Wrap a payload in a ``[len:u32][payload]`` frame."""
    return struct.pack("<I", len(payload)) + payload
