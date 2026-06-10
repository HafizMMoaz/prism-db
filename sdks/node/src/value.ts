// The scalar value model and its tagged/untagged wire codec.
//
// Mirrors `crates/prism-protocol/src/data.rs` (`Value`) and the type tags in
// `docs/specs/record-format.md`. The SDK accepts plain JS values and maps them
// to wire types; on decode it returns plain JS values.

import { Reader, Writer } from "./codec.js";
import { ProtocolError } from "./errors.js";

// Record-format type tags.
export const TAG = {
  NULL: 0x00,
  BOOL: 0x01,
  INT32: 0x02,
  INT64: 0x03,
  DOUBLE: 0x04,
  STRING: 0x05,
  BINARY: 0x06,
  TIMESTAMP: 0x09,
  OBJECTID: 0x0a,
} as const;

/** A 12-byte document identifier. */
export class ObjectId {
  readonly bytes: Buffer;

  constructor(bytes: Buffer | Uint8Array) {
    if (bytes.length !== 12) throw new ProtocolError("ObjectId must be 12 bytes");
    this.bytes = Buffer.from(bytes);
  }

  /** Lowercase 24-character hex. */
  toHex(): string {
    return this.bytes.toString("hex");
  }

  static fromHex(hex: string): ObjectId {
    if (hex.length !== 24) throw new ProtocolError("ObjectId hex must be 24 chars");
    return new ObjectId(Buffer.from(hex, "hex"));
  }

  toString(): string {
    return this.toHex();
  }
}

/**
 * An explicitly-typed value, for cases where the default mapping of a JS value
 * is not what you want (e.g. a 32-bit int, a float that happens to be integral,
 * or a timestamp). Build with {@link int32}/{@link int64}/{@link float64}/
 * {@link timestamp}.
 */
export class Typed {
  constructor(
    readonly tag: number,
    readonly value: number | bigint,
  ) {}
}

/** Force a value to wire `Int32`. */
export const int32 = (n: number): Typed => new Typed(TAG.INT32, n | 0);
/** Force a value to wire `Int64`. */
export const int64 = (n: number | bigint): Typed => new Typed(TAG.INT64, BigInt(n));
/** Force a value to wire `Double`. */
export const float64 = (n: number): Typed => new Typed(TAG.DOUBLE, n);
/** Force a value to wire `Timestamp` (microseconds since the Unix epoch). */
export const timestamp = (us: number | bigint): Typed => new Typed(TAG.TIMESTAMP, BigInt(us));

/**
 * A value the SDK can send. Plain JS values map as: `null`→Null,
 * `boolean`→Bool, `bigint`→Int64, integer `number`→Int64, non-integer
 * `number`→Double, `string`→Str, `Uint8Array`→Binary, `ObjectId`→ObjectId.
 * Wrap with {@link int32}/{@link float64}/{@link timestamp} for other types.
 */
export type Value =
  | null
  | boolean
  | number
  | bigint
  | string
  | Uint8Array
  | ObjectId
  | Typed;

/** Resolve a JS value to its wire type tag. */
export function tagOf(v: Value): number {
  if (v === null) return TAG.NULL;
  if (v instanceof Typed) return v.tag;
  if (v instanceof ObjectId) return TAG.OBJECTID;
  switch (typeof v) {
    case "boolean":
      return TAG.BOOL;
    case "bigint":
      return TAG.INT64;
    case "number":
      return Number.isInteger(v) ? TAG.INT64 : TAG.DOUBLE;
    case "string":
      return TAG.STRING;
    case "object":
      if (v instanceof Uint8Array) return TAG.BINARY;
  }
  throw new ProtocolError(`unsupported value: ${String(v)}`);
}

/** Write the value bytes for a known `tag` (no tag byte). */
export function encodeUntagged(w: Writer, tag: number, v: Value): void {
  const raw = v instanceof Typed ? v.value : v;
  switch (tag) {
    case TAG.NULL:
      break;
    case TAG.BOOL:
      w.u8(raw ? 1 : 0);
      break;
    case TAG.INT32:
      w.i32(Number(raw));
      break;
    case TAG.INT64:
      w.i64(typeof raw === "bigint" ? raw : BigInt(raw as number));
      break;
    case TAG.DOUBLE:
      w.f64(Number(raw));
      break;
    case TAG.TIMESTAMP:
      w.i64(typeof raw === "bigint" ? raw : BigInt(raw as number));
      break;
    case TAG.STRING:
      w.strU32(String(raw));
      break;
    case TAG.OBJECTID:
      w.raw((raw as ObjectId).bytes);
      break;
    case TAG.BINARY: {
      const bytes = raw as Uint8Array;
      w.u32(bytes.length);
      w.u8(0); // subtype
      w.raw(bytes);
      break;
    }
    default:
      throw new ProtocolError(`cannot encode value tag 0x${tag.toString(16)}`);
  }
}

/** Write a tagged value (tag byte then the value bytes). */
export function encodeTagged(w: Writer, v: Value): void {
  const tag = tagOf(v);
  w.u8(tag);
  encodeUntagged(w, tag, v);
}

/** Read the value bytes for a known `tag`, returning a plain JS value. */
export function decodeUntagged(r: Reader, tag: number): Value {
  switch (tag) {
    case TAG.NULL:
      return null;
    case TAG.BOOL:
      return r.u8() !== 0;
    case TAG.INT32:
      return r.i32();
    case TAG.INT64:
      return r.i64();
    case TAG.DOUBLE:
      return r.f64();
    case TAG.TIMESTAMP:
      return r.i64();
    case TAG.STRING:
      return r.strU32();
    case TAG.OBJECTID:
      return new ObjectId(r.raw(12));
    case TAG.BINARY: {
      const len = r.u32();
      r.u8(); // subtype (discarded)
      return Buffer.from(r.raw(len));
    }
    default:
      throw new ProtocolError(`unknown value tag 0x${tag.toString(16)}`);
  }
}

/** Read a tagged value. */
export function decodeTagged(r: Reader): Value {
  return decodeUntagged(r, r.u8());
}
