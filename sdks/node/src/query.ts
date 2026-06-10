// The document query filter and its wire encoding.
//
// Mirrors `prism_protocol::DocQuery` (crates/prism-protocol/src/data.rs): a
// tag byte, then the operator-specific body. Operand values reuse the tagged
// `Value` encoding. Build queries with the `Q` helpers.

import { Writer } from "./codec.js";
import { ProtocolError } from "./errors.js";
import { Value, encodeTagged } from "./value.js";

// Discriminant tags, identical to the Rust `DocQuery::encode`.
const Q_ALL = 0;
const Q_EQ = 1;
const Q_NE = 2;
const Q_GT = 3;
const Q_LT = 4;
const Q_GTE = 5;
const Q_LTE = 6;
const Q_IN = 7;
const Q_NIN = 8;
const Q_EXISTS = 9;
const Q_AND = 10;
const Q_OR = 11;
const Q_NOT = 12;

/** A document query filter. Construct via the {@link Q} helpers. */
export type DocQuery =
  | { kind: "all" }
  | { kind: "field"; tag: number; field: string; value: Value }
  | { kind: "set"; tag: number; field: string; values: Value[] }
  | { kind: "exists"; field: string; present: boolean }
  | { kind: "group"; tag: number; subs: DocQuery[] }
  | { kind: "not"; sub: DocQuery };

/** Query builders mirroring the engine's filter set. */
export const Q = {
  /** Match every document. */
  all(): DocQuery {
    return { kind: "all" };
  },
  eq(field: string, value: Value): DocQuery {
    return { kind: "field", tag: Q_EQ, field, value };
  },
  ne(field: string, value: Value): DocQuery {
    return { kind: "field", tag: Q_NE, field, value };
  },
  gt(field: string, value: Value): DocQuery {
    return { kind: "field", tag: Q_GT, field, value };
  },
  lt(field: string, value: Value): DocQuery {
    return { kind: "field", tag: Q_LT, field, value };
  },
  gte(field: string, value: Value): DocQuery {
    return { kind: "field", tag: Q_GTE, field, value };
  },
  lte(field: string, value: Value): DocQuery {
    return { kind: "field", tag: Q_LTE, field, value };
  },
  in(field: string, values: Value[]): DocQuery {
    return { kind: "set", tag: Q_IN, field, values };
  },
  nin(field: string, values: Value[]): DocQuery {
    return { kind: "set", tag: Q_NIN, field, values };
  },
  exists(field: string, present = true): DocQuery {
    return { kind: "exists", field, present };
  },
  and(...subs: DocQuery[]): DocQuery {
    return { kind: "group", tag: Q_AND, subs };
  },
  or(...subs: DocQuery[]): DocQuery {
    return { kind: "group", tag: Q_OR, subs };
  },
  not(sub: DocQuery): DocQuery {
    return { kind: "not", sub };
  },
} as const;

function encodeQuery(w: Writer, q: DocQuery): void {
  switch (q.kind) {
    case "all":
      w.u8(Q_ALL);
      break;
    case "field":
      w.u8(q.tag);
      w.strU16(q.field);
      encodeTagged(w, q.value);
      break;
    case "set":
      w.u8(q.tag);
      w.strU16(q.field);
      if (q.values.length > 0xffffffff) throw new ProtocolError("query set too large");
      w.u32(q.values.length);
      for (const v of q.values) encodeTagged(w, v);
      break;
    case "exists":
      w.u8(Q_EXISTS);
      w.strU16(q.field);
      w.u8(q.present ? 1 : 0);
      break;
    case "group":
      w.u8(q.tag);
      w.u32(q.subs.length);
      for (const s of q.subs) encodeQuery(w, s);
      break;
    case "not":
      w.u8(Q_NOT);
      encodeQuery(w, q.sub);
      break;
  }
}

/** Encode a query to the standalone bytes carried in a document command. */
export function encodeDocQuery(q: DocQuery): Buffer {
  const w = new Writer();
  encodeQuery(w, q);
  return Buffer.from(w.out());
}
