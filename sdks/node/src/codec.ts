// Low-level binary codec: a growable little-endian `Writer`, a bounds-checked
// `Reader`, and the length-prefixed frame helpers. The byte layouts mirror
// `crates/prism-protocol/src/codec.rs` exactly (all multi-byte integers LE).

import { ProtocolError } from "./errors.js";

/** A growable little-endian writer over a Node `Buffer`. */
export class Writer {
  private buf: Buffer;
  private len = 0;

  constructor(capacity = 64) {
    this.buf = Buffer.allocUnsafe(capacity);
  }

  private ensure(extra: number): void {
    const needed = this.len + extra;
    if (needed <= this.buf.length) return;
    let cap = this.buf.length * 2;
    while (cap < needed) cap *= 2;
    const next = Buffer.allocUnsafe(cap);
    this.buf.copy(next, 0, 0, this.len);
    this.buf = next;
  }

  u8(v: number): void {
    this.ensure(1);
    this.buf.writeUInt8(v & 0xff, this.len);
    this.len += 1;
  }

  u16(v: number): void {
    this.ensure(2);
    this.buf.writeUInt16LE(v & 0xffff, this.len);
    this.len += 2;
  }

  u32(v: number): void {
    this.ensure(4);
    this.buf.writeUInt32LE(v >>> 0, this.len);
    this.len += 4;
  }

  i32(v: number): void {
    this.ensure(4);
    this.buf.writeInt32LE(v | 0, this.len);
    this.len += 4;
  }

  u64(v: bigint): void {
    this.ensure(8);
    this.buf.writeBigUInt64LE(BigInt.asUintN(64, v), this.len);
    this.len += 8;
  }

  i64(v: bigint): void {
    this.ensure(8);
    this.buf.writeBigInt64LE(BigInt.asIntN(64, v), this.len);
    this.len += 8;
  }

  f64(v: number): void {
    this.ensure(8);
    this.buf.writeDoubleLE(v, this.len);
    this.len += 8;
  }

  /** A 128-bit unsigned integer as 16 little-endian bytes. */
  u128(v: bigint): void {
    this.ensure(16);
    const x = BigInt.asUintN(128, v);
    const mask = (1n << 64n) - 1n;
    this.buf.writeBigUInt64LE(x & mask, this.len);
    this.buf.writeBigUInt64LE((x >> 64n) & mask, this.len + 8);
    this.len += 16;
  }

  raw(bytes: Buffer | Uint8Array): void {
    this.ensure(bytes.length);
    Buffer.from(bytes.buffer, bytes.byteOffset, bytes.length).copy(this.buf, this.len);
    this.len += bytes.length;
  }

  /** A UTF-8 string with a u16 length prefix. */
  strU16(s: string): void {
    const b = Buffer.from(s, "utf8");
    this.u16(b.length);
    this.raw(b);
  }

  /** A UTF-8 string with a u32 length prefix. */
  strU32(s: string): void {
    const b = Buffer.from(s, "utf8");
    this.u32(b.length);
    this.raw(b);
  }

  /** A byte string with a u16 length prefix. */
  bytesU16(b: Buffer | Uint8Array): void {
    this.u16(b.length);
    this.raw(b);
  }

  /** A byte string with a u32 length prefix. */
  bytesU32(b: Buffer | Uint8Array): void {
    this.u32(b.length);
    this.raw(b);
  }

  /** The written bytes (a view into the internal buffer). */
  out(): Buffer {
    return this.buf.subarray(0, this.len);
  }
}

/** A bounds-checked little-endian reader over a `Buffer`. */
export class Reader {
  private p = 0;

  constructor(private readonly buf: Buffer) {}

  private need(n: number): void {
    if (this.p + n > this.buf.length) {
      throw new ProtocolError(`truncated: need ${n} bytes at offset ${this.p}`);
    }
  }

  u8(): number {
    this.need(1);
    const v = this.buf.readUInt8(this.p);
    this.p += 1;
    return v;
  }

  u16(): number {
    this.need(2);
    const v = this.buf.readUInt16LE(this.p);
    this.p += 2;
    return v;
  }

  u32(): number {
    this.need(4);
    const v = this.buf.readUInt32LE(this.p);
    this.p += 4;
    return v;
  }

  i32(): number {
    this.need(4);
    const v = this.buf.readInt32LE(this.p);
    this.p += 4;
    return v;
  }

  u64(): bigint {
    this.need(8);
    const v = this.buf.readBigUInt64LE(this.p);
    this.p += 8;
    return v;
  }

  i64(): bigint {
    this.need(8);
    const v = this.buf.readBigInt64LE(this.p);
    this.p += 8;
    return v;
  }

  f64(): number {
    this.need(8);
    const v = this.buf.readDoubleLE(this.p);
    this.p += 8;
    return v;
  }

  u128(): bigint {
    this.need(16);
    const lo = this.buf.readBigUInt64LE(this.p);
    const hi = this.buf.readBigUInt64LE(this.p + 8);
    this.p += 16;
    return (hi << 64n) | lo;
  }

  raw(n: number): Buffer {
    this.need(n);
    const s = this.buf.subarray(this.p, this.p + n);
    this.p += n;
    return s;
  }

  strU16(): string {
    return this.raw(this.u16()).toString("utf8");
  }

  strU32(): string {
    return this.raw(this.u32()).toString("utf8");
  }

  bytesU16(): Buffer {
    return Buffer.from(this.raw(this.u16()));
  }

  bytesU32(): Buffer {
    return Buffer.from(this.raw(this.u32()));
  }

  remaining(): number {
    return this.buf.length - this.p;
  }

  /** Throw unless every byte has been consumed. */
  expectEnd(): void {
    if (this.remaining() !== 0) {
      throw new ProtocolError(`${this.remaining()} trailing byte(s) after message`);
    }
  }
}

/** Wrap a payload in a `[len:u32][payload]` frame. */
export function frameEncode(payload: Buffer): Buffer {
  const out = Buffer.allocUnsafe(4 + payload.length);
  out.writeUInt32LE(payload.length, 0);
  payload.copy(out, 4);
  return out;
}

/**
 * Parse one frame from the front of `buf`. Returns the payload and the number
 * of bytes consumed, or `null` if a full frame has not yet arrived.
 */
export function frameParse(buf: Buffer): { payload: Buffer; consumed: number } | null {
  if (buf.length < 4) return null;
  const len = buf.readUInt32LE(0);
  if (buf.length < 4 + len) return null;
  return { payload: buf.subarray(4, 4 + len), consumed: 4 + len };
}
