import assert from "node:assert/strict";
import { test } from "node:test";

import { Reader, Writer, frameEncode, frameParse } from "../src/codec.js";

test("writer/reader round-trip scalars", () => {
  const w = new Writer();
  w.u8(0xab);
  w.u16(0x1234);
  w.u32(0xdeadbeef);
  w.i32(-5);
  w.u64(0xfedcba9876543210n);
  w.i64(-1234567890123n);
  w.f64(3.5);
  w.u128((1n << 100n) | 7n);
  w.strU16("héllo");
  w.strU32("world");
  w.bytesU16(Buffer.from([1, 2, 3]));

  const r = new Reader(w.out());
  assert.equal(r.u8(), 0xab);
  assert.equal(r.u16(), 0x1234);
  assert.equal(r.u32(), 0xdeadbeef);
  assert.equal(r.i32(), -5);
  assert.equal(r.u64(), 0xfedcba9876543210n);
  assert.equal(r.i64(), -1234567890123n);
  assert.equal(r.f64(), 3.5);
  assert.equal(r.u128(), (1n << 100n) | 7n);
  assert.equal(r.strU16(), "héllo");
  assert.equal(r.strU32(), "world");
  assert.deepEqual([...r.bytesU16()], [1, 2, 3]);
  r.expectEnd();
});

test("little-endian byte layout is exact", () => {
  const w = new Writer();
  w.u32(0x01020304);
  assert.deepEqual([...w.out()], [0x04, 0x03, 0x02, 0x01]);
});

test("frame round-trip and length prefix", () => {
  const payload = Buffer.from([9, 8, 7]);
  const framed = frameEncode(payload);
  assert.deepEqual([...framed.subarray(0, 4)], [3, 0, 0, 0]);
  const parsed = frameParse(framed);
  assert.ok(parsed);
  assert.equal(parsed.consumed, framed.length);
  assert.deepEqual([...parsed.payload], [9, 8, 7]);
});

test("frameParse needs a full frame", () => {
  const framed = frameEncode(Buffer.from([1, 2, 3, 4]));
  assert.equal(frameParse(framed.subarray(0, 2)), null); // partial length
  assert.equal(frameParse(framed.subarray(0, framed.length - 1)), null); // partial body
  assert.ok(frameParse(framed));
});

test("frameParse handles back-to-back frames", () => {
  const a = frameEncode(Buffer.from([0xaa]));
  const b = frameEncode(Buffer.from([0xbb, 0xcc]));
  const buf = Buffer.concat([a, b]);

  const first = frameParse(buf)!;
  assert.deepEqual([...first.payload], [0xaa]);
  const second = frameParse(buf.subarray(first.consumed))!;
  assert.deepEqual([...second.payload], [0xbb, 0xcc]);
});

test("reader rejects truncation", () => {
  const r = new Reader(Buffer.from([1, 2]));
  assert.throws(() => r.u32());
});
