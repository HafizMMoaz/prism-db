// The document tagged-binary codec.
//
// Mirrors `crates/prism-doc/src/value.rs` (`Document::encode`/`decode`). A
// document is `[total:u32][count:u16]` followed by, per field,
// `[tag:u8][nameLen:u16][name][value bytes]`. Field value bytes use the same
// encoding as scalar values, except documents have no Binary type.

import { Reader, Writer } from "./codec.js";
import { ProtocolError } from "./errors.js";
import { TAG, Value, decodeUntagged, encodeUntagged, tagOf } from "./value.js";

/** A document is a plain object; field insertion order is preserved. */
export type Document = Record<string, Value>;

/** Encode a document to its tagged-binary payload. */
export function encodeDocument(doc: Document): Buffer {
  const body = new Writer();
  const entries = Object.entries(doc);
  if (entries.length > 0xffff) throw new ProtocolError("too many document fields");
  body.u16(entries.length);
  for (const [name, value] of entries) {
    const tag = tagOf(value);
    if (tag === TAG.BINARY) {
      throw new ProtocolError(`field "${name}": binary values are not supported in documents`);
    }
    body.u8(tag);
    body.strU16(name);
    encodeUntagged(body, tag, value);
  }
  const inner = body.out();
  const out = new Writer(inner.length + 4);
  out.u32(4 + inner.length); // total length, including this u32
  out.raw(inner);
  return Buffer.from(out.out());
}

/** Decode a document from its tagged-binary payload. */
export function decodeDocument(bytes: Buffer): Document {
  const r = new Reader(bytes);
  r.u32(); // total length (redundant with the frame's blob length)
  const count = r.u16();
  const doc: Document = {};
  for (let i = 0; i < count; i++) {
    const tag = r.u8();
    const name = r.strU16();
    doc[name] = decodeUntagged(r, tag);
  }
  return doc;
}
