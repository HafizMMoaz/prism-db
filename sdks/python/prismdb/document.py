"""The document tagged-binary codec.

Mirrors ``crates/prism-doc/src/value.rs`` (``Document::encode``/``decode``). A
document is ``[total:u32][count:u16]`` followed by, per field,
``[tag:u8][nameLen:u16][name][value bytes]``. Field value bytes use the same
encoding as scalar values, except documents have no Binary type."""

from __future__ import annotations

from typing import Dict

from ._codec import Reader, Writer
from .errors import ProtocolError
from .value import TAG, Value, decode_untagged, encode_untagged, tag_of

# A document is a plain dict; field insertion order is preserved.
Document = Dict[str, Value]


def encode_document(doc: Document) -> bytes:
    """Encode a document to its tagged-binary payload."""
    body = Writer()
    if len(doc) > 0xFFFF:
        raise ProtocolError("too many document fields")
    body.u16(len(doc))
    for name, value in doc.items():
        tag = tag_of(value)
        if tag == TAG.BINARY:
            raise ProtocolError(f'field "{name}": binary values are not supported in documents')
        body.u8(tag)
        body.str_u16(name)
        encode_untagged(body, tag, value)
    inner = body.out()
    out = Writer()
    out.u32(4 + len(inner))  # total length, including this u32
    out.raw(inner)
    return out.out()


def decode_document(raw: bytes) -> Document:
    """Decode a document from its tagged-binary payload."""
    r = Reader(raw)
    r.u32()  # total length (redundant with the frame's blob length)
    count = r.u16()
    doc: Document = {}
    for _ in range(count):
        tag = r.u8()
        name = r.str_u16()
        doc[name] = decode_untagged(r, tag)
    return doc
