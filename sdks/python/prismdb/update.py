"""Document update operators and their wire encoding.

Mirrors ``prism_protocol::DocUpdate``: an ordered list of $set / $unset / $inc
mutations. Build with the :class:`U` helpers. Operand values reuse the tagged
``Value`` encoding. Carried as the ``update`` blob of an update command."""

from __future__ import annotations

from typing import List, Union

from ._codec import Writer
from .value import Value, encode_tagged


class DocUpdate:
    """One field mutation. Construct via the :class:`U` helpers."""

    __slots__ = ("op", "field", "value", "delta")

    def __init__(self, op: str, field: str, value: Value = None, delta: int = 0) -> None:
        self.op = op
        self.field = field
        self.value = value
        self.delta = delta


class U:
    """Update builders mirroring the engine's update operators."""

    @staticmethod
    def set(field: str, value: Value) -> DocUpdate:
        """``$set`` - set ``field`` to ``value``."""
        return DocUpdate("set", field, value=value)

    @staticmethod
    def unset(field: str) -> DocUpdate:
        """``$unset`` - remove ``field``."""
        return DocUpdate("unset", field)

    @staticmethod
    def inc(field: str, delta: Union[int, float]) -> DocUpdate:
        """``$inc`` - add ``delta`` to the integer ``field``."""
        return DocUpdate("inc", field, delta=int(delta))


def encode_doc_update(ops: List[DocUpdate]) -> bytes:
    """Encode a list of update operations to the ``update`` blob of a command."""
    w = Writer()
    w.u32(len(ops))
    for op in ops:
        if op.op == "set":
            w.u8(1)
            w.str_u16(op.field)
            encode_tagged(w, op.value)
        elif op.op == "unset":
            w.u8(2)
            w.str_u16(op.field)
        elif op.op == "inc":
            w.u8(3)
            w.str_u16(op.field)
            w.i64(int(op.delta))
    return w.out()
