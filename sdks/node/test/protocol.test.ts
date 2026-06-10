import assert from "node:assert/strict";
import { test } from "node:test";

import { Reader, Writer } from "../src/codec.js";
import { decodeDocument, encodeDocument } from "../src/document.js";
import { decodePacket, encodePacket } from "../src/messages.js";
import { Q, encodeDocQuery } from "../src/query.js";
import {
  ObjectId,
  TAG,
  Value,
  decodeTagged,
  encodeTagged,
  float64,
  int32,
  timestamp,
} from "../src/value.js";

function roundTripValue(v: Value): Value {
  const w = new Writer();
  encodeTagged(w, v);
  return decodeTagged(new Reader(w.out()));
}

test("tagged value round-trips for every scalar", () => {
  assert.equal(roundTripValue(null), null);
  assert.equal(roundTripValue(true), true);
  assert.equal(roundTripValue("hello"), "hello");
  assert.equal(roundTripValue(int32(-7)), -7);
  assert.equal(roundTripValue(123n), 123n); // bigint -> Int64
  assert.equal(roundTripValue(42), 42n); // integer number -> Int64
  assert.equal(roundTripValue(float64(2.5)), 2.5);
  assert.equal(roundTripValue(timestamp(1_700_000_000_000_000n)), 1_700_000_000_000_000n);
  const oid = ObjectId.fromHex("0123456789abcdef01234567");
  const back = roundTripValue(oid);
  assert.ok(back instanceof ObjectId);
  assert.equal((back as ObjectId).toHex(), "0123456789abcdef01234567");
});

test("document round-trips through the tagged-binary codec", () => {
  const doc = {
    _id: ObjectId.fromHex("aabbccddeeff001122334455"),
    name: "alice",
    age: 30n,
    active: true,
    score: float64(9.5),
    nickname: null,
  };
  const decoded = decodeDocument(encodeDocument(doc));
  assert.equal(decoded.name, "alice");
  assert.equal(decoded.age, 30n);
  assert.equal(decoded.active, true);
  assert.equal(decoded.score, 9.5);
  assert.equal(decoded.nickname, null);
  assert.ok(decoded._id instanceof ObjectId);
});

test("document length prefix equals total bytes", () => {
  const bytes = encodeDocument({ a: 1n });
  assert.equal(bytes.readUInt32LE(0), bytes.length);
});

test("DocQuery encodes to the exact wire bytes", () => {
  // Q.eq("a", int32(1)) =>
  //   tag 0x01 | field strU16 "a" (01 00 'a') | value tagged INT32 (02 01 00 00 00)
  const bytes = encodeDocQuery(Q.eq("a", int32(1)));
  assert.equal(bytes.toString("hex"), "0101006102" + "01000000");
  // Q.all() is a single zero byte.
  assert.equal(encodeDocQuery(Q.all()).toString("hex"), "00");
});

test("packet header places type and request_id correctly", () => {
  const payload = encodePacket(0xabcd, { type: "ping" });
  assert.equal(payload[0], 0x70); // Ping
  assert.deepEqual([...payload.subarray(1, 4)], [0, 0, 0]);
  assert.deepEqual([...payload.subarray(4, 8)], [0xcd, 0xab, 0, 0]); // request_id LE
  assert.deepEqual([...payload.subarray(8, 12)], [0, 0, 0, 0]);
});

test("decode a hand-built Pong and SqlResult", () => {
  // Pong: header only.
  const pong = Buffer.concat([Buffer.from([0x71, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0])]);
  const p = decodePacket(pong);
  assert.equal(p.requestId, 5);
  assert.equal(p.message.type, "pong");

  // SqlResult: status 0, affected 0, 1 column "n" INT64 not-null, 1 row value 7.
  const w = new Writer();
  w.u8(0x21);
  w.raw(Buffer.from([0, 0, 0]));
  w.u32(1); // request_id
  w.raw(Buffer.from([0, 0, 0, 0]));
  w.u8(0); // status
  w.u64(0n); // affected
  w.u16(1); // column_count
  w.strU16("n"); // column name
  w.u8(TAG.INT64); // type tag
  w.u8(0); // nullable = false
  w.u32(1); // row_count
  w.raw(Buffer.from([0x00])); // null bitmap (1 col -> 1 byte, not null)
  w.i64(7n); // the value
  w.u8(0); // more_frames
  const sql = decodePacket(w.out());
  assert.equal(sql.message.type, "sqlResult");
  if (sql.message.type === "sqlResult") {
    assert.equal(sql.message.columns[0]!.name, "n");
    assert.equal(sql.message.rows[0]![0], 7n);
  }
});
