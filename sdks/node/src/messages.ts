// Protocol messages: the 12-byte header plus per-message bodies.
//
// Mirrors `crates/prism-protocol/src/message.rs`. The SDK *encodes* the client
// messages and *decodes* the server messages; both directions share the header
// and the error trailer.

import { Reader, Writer } from "./codec.js";
import { ErrorInfo, ProtocolError } from "./errors.js";
import { ObjectId, Value, decodeUntagged, encodeTagged } from "./value.js";

// Message-type discriminants (first header byte).
export const MSG = {
  HELLO: 0x01,
  HELLO_ACK: 0x02,
  AUTH: 0x03,
  AUTH_ACK: 0x04,
  BEGIN: 0x10,
  COMMIT: 0x11,
  ABORT: 0x12,
  TXN_ACK: 0x13,
  SQL_EXECUTE: 0x20,
  SQL_RESULT: 0x21,
  DOC_OP: 0x30,
  DOC_RESULT: 0x31,
  KV_OP: 0x40,
  KV_RESULT: 0x41,
  CANCEL: 0x50,
  NOTICE: 0x60,
  PING: 0x70,
  PONG: 0x71,
} as const;

export const AUTH_PASSWORD = 1;
export const AUTH_MTLS = 2;
export const TXN_READ_WRITE = 0;
export const TXN_READ_ONLY = 1;

/** `Hello` feature bit: the body carries a connect-time database name. */
export const FEATURE_CONNECT_DB = 1 << 0;

// ---- document / kv sub-commands ---------------------------------------------

export type DocCommand =
  | { op: 1; doc: Buffer } // insertOne
  | { op: 2; docs: Buffer[] } // insertMany
  | { op: 3; query: Buffer; options: Buffer } // find
  | { op: 4; query: Buffer; options: Buffer } // findOne
  | { op: 5; query: Buffer; update: Buffer; options: Buffer } // updateOne
  | { op: 6; query: Buffer; update: Buffer; options: Buffer } // updateMany
  | { op: 7; query: Buffer; options: Buffer } // deleteOne
  | { op: 8; query: Buffer; options: Buffer } // deleteMany
  | { op: 9; query: Buffer; options: Buffer }; // count

export type KvCommand =
  | { op: 1; key: Buffer } // get
  | { op: 2; key: Buffer; value: Buffer } // put
  | { op: 3; key: Buffer } // delete
  | { op: 4; start: Buffer; end: Buffer; maxResults: number } // range
  | { op: 5; prefix: Buffer; maxResults: number }; // scan

// ---- outgoing (client → server) ---------------------------------------------

export type ClientMessage =
  | { type: "hello"; protocolVersion: number; clientName: string; clientVersion: string; features: number; database: string }
  | { type: "auth"; mechanism: number; username: string; password: string }
  | { type: "begin"; mode: number }
  | { type: "commit"; idempotencyKey: bigint }
  | { type: "abort" }
  | { type: "sqlExecute"; sql: string; params: Value[]; options: number }
  | { type: "docOp"; collection: string; command: DocCommand }
  | { type: "kvOp"; namespace: string; command: KvCommand }
  | { type: "ping" };

// ---- incoming (server → client) ---------------------------------------------

export interface ColumnDesc {
  name: string;
  typeTag: number;
  nullable: boolean;
}

export type KvResultBody =
  | { op: 1; value: Buffer | null }
  | { op: 2 }
  | { op: 3 }
  | { op: 4; entries: Array<[Buffer, Buffer]>; moreFrames: boolean }
  | { op: 5; entries: Array<[Buffer, Buffer]>; moreFrames: boolean };

export type ServerMessage =
  | { type: "helloAck"; status: number; serverVersion: string; features: number; sessionId: bigint; error?: ErrorInfo }
  | { type: "authAck"; status: number; userOid: bigint; error?: ErrorInfo }
  | { type: "txnAck"; status: number; txnId: bigint; commitLsn: bigint; error?: ErrorInfo }
  | {
      type: "sqlResult";
      status: number;
      affectedRows: bigint;
      columns: ColumnDesc[];
      rows: Array<Array<Value | null>>;
      moreFrames: boolean;
      error?: ErrorInfo;
    }
  | {
      type: "docResult";
      status: number;
      affected: bigint;
      insertedIds: ObjectId[];
      docs: Buffer[];
      moreFrames: boolean;
      error?: ErrorInfo;
    }
  | { type: "kvResult"; status: number; body: KvResultBody; error?: ErrorInfo }
  | { type: "notice"; severity: number; code: number; message: string }
  | { type: "pong" };

const RESERVED3 = Buffer.alloc(3);
const RESERVED4 = Buffer.alloc(4);

function typeCode(m: ClientMessage): number {
  switch (m.type) {
    case "hello":
      return MSG.HELLO;
    case "auth":
      return MSG.AUTH;
    case "begin":
      return MSG.BEGIN;
    case "commit":
      return MSG.COMMIT;
    case "abort":
      return MSG.ABORT;
    case "sqlExecute":
      return MSG.SQL_EXECUTE;
    case "docOp":
      return MSG.DOC_OP;
    case "kvOp":
      return MSG.KV_OP;
    case "ping":
      return MSG.PING;
  }
}

/** Encode a client message into a payload (12-byte header + body). */
export function encodePacket(requestId: number, m: ClientMessage): Buffer {
  const w = new Writer();
  w.u8(typeCode(m));
  w.raw(RESERVED3);
  w.u32(requestId);
  w.raw(RESERVED4);
  encodeBody(w, m);
  return Buffer.from(w.out());
}

function encodeBody(w: Writer, m: ClientMessage): void {
  switch (m.type) {
    case "hello":
      w.u32(m.protocolVersion);
      w.strU16(m.clientName);
      w.strU16(m.clientVersion);
      w.u32(m.features);
      // The database field only travels under its feature bit (matches the Rust
      // codec), so a no-database Hello stays byte-compatible with v1.
      if (m.features & FEATURE_CONNECT_DB) w.strU16(m.database);
      break;
    case "auth":
      w.u8(m.mechanism);
      w.strU16(m.username);
      if (m.mechanism === AUTH_PASSWORD) w.strU16(m.password);
      break;
    case "begin":
      w.u8(m.mode);
      break;
    case "commit":
      w.u128(m.idempotencyKey);
      break;
    case "abort":
      break;
    case "sqlExecute":
      w.strU32(m.sql);
      w.u16(m.params.length);
      for (const p of m.params) encodeTagged(w, p);
      w.u32(m.options);
      break;
    case "docOp":
      w.u8(m.command.op);
      w.strU16(m.collection);
      encodeDocBody(w, m.command);
      break;
    case "kvOp":
      w.u8(m.command.op);
      w.strU16(m.namespace);
      encodeKvBody(w, m.command);
      break;
    case "ping":
      break;
  }
}

function encodeDocBody(w: Writer, c: DocCommand): void {
  switch (c.op) {
    case 1:
      w.bytesU32(c.doc);
      break;
    case 2:
      w.u32(c.docs.length);
      for (const d of c.docs) w.bytesU32(d);
      break;
    case 3:
    case 4:
      w.bytesU32(c.query);
      w.bytesU32(c.options);
      break;
    case 5:
    case 6:
      w.bytesU32(c.query);
      w.bytesU32(c.update);
      w.bytesU32(c.options);
      break;
    case 7:
    case 8:
    case 9:
      w.bytesU32(c.query);
      w.bytesU32(c.options);
      break;
  }
}

function encodeKvBody(w: Writer, c: KvCommand): void {
  switch (c.op) {
    case 1:
    case 3:
      w.bytesU16(c.key);
      break;
    case 2:
      w.bytesU16(c.key);
      w.bytesU32(c.value);
      break;
    case 4:
      w.bytesU16(c.start);
      w.bytesU16(c.end);
      w.u32(c.maxResults);
      break;
    case 5:
      w.bytesU16(c.prefix);
      w.u32(c.maxResults);
      break;
  }
}

// ---- decoding (server → client) ---------------------------------------------

/** A decoded server packet: the echoed request id and the message. */
export interface ServerPacket {
  requestId: number;
  message: ServerMessage;
}

/** Decode a payload (header + body) into a server packet. */
export function decodePacket(payload: Buffer): ServerPacket {
  const r = new Reader(payload);
  const type = r.u8();
  r.raw(3);
  const requestId = r.u32();
  r.raw(4);
  const message = decodeBody(type, r);
  r.expectEnd();
  return { requestId, message };
}

function decodeTrailer(r: Reader, status: number): ErrorInfo | undefined {
  if (status === 0) return undefined;
  return {
    code: r.u32(),
    message: r.strU16(),
    sqlstate: r.raw(5).toString("ascii"),
    detail: r.strU16(),
    position: r.u32(),
  };
}

function decodeBody(type: number, r: Reader): ServerMessage {
  switch (type) {
    case MSG.HELLO_ACK: {
      const status = r.u8();
      const serverVersion = r.strU16();
      const features = r.u32();
      const sessionId = r.u128();
      return { type: "helloAck", status, serverVersion, features, sessionId, error: decodeTrailer(r, status) };
    }
    case MSG.AUTH_ACK: {
      const status = r.u8();
      const userOid = r.u64();
      return { type: "authAck", status, userOid, error: decodeTrailer(r, status) };
    }
    case MSG.TXN_ACK: {
      const status = r.u8();
      const txnId = r.u64();
      const commitLsn = r.u64();
      return { type: "txnAck", status, txnId, commitLsn, error: decodeTrailer(r, status) };
    }
    case MSG.SQL_RESULT: {
      const status = r.u8();
      const affectedRows = r.u64();
      const colCount = r.u16();
      const columns: ColumnDesc[] = [];
      for (let i = 0; i < colCount; i++) {
        columns.push({ name: r.strU16(), typeTag: r.u8(), nullable: r.u8() !== 0 });
      }
      const rowCount = r.u32();
      const rows = decodeRows(columns, rowCount, r);
      const moreFrames = r.u8() !== 0;
      return { type: "sqlResult", status, affectedRows, columns, rows, moreFrames, error: decodeTrailer(r, status) };
    }
    case MSG.DOC_RESULT: {
      const status = r.u8();
      const affected = r.u64();
      const idCount = r.u32();
      const insertedIds: ObjectId[] = [];
      for (let i = 0; i < idCount; i++) insertedIds.push(new ObjectId(r.raw(12)));
      const docCount = r.u32();
      const docs: Buffer[] = [];
      for (let i = 0; i < docCount; i++) docs.push(r.bytesU32());
      const moreFrames = r.u8() !== 0;
      return { type: "docResult", status, affected, insertedIds, docs, moreFrames, error: decodeTrailer(r, status) };
    }
    case MSG.KV_RESULT: {
      const status = r.u8();
      const opType = r.u8();
      const body = decodeKvBody(opType, r);
      return { type: "kvResult", status, body, error: decodeTrailer(r, status) };
    }
    case MSG.NOTICE:
      return { type: "notice", severity: r.u8(), code: r.u32(), message: r.strU16() };
    case MSG.PONG:
      return { type: "pong" };
    default:
      throw new ProtocolError(`unexpected server message type 0x${type.toString(16)}`);
  }
}

function decodeRows(columns: ColumnDesc[], rowCount: number, r: Reader): Array<Array<Value | null>> {
  const nb = Math.ceil(columns.length / 8);
  const rows: Array<Array<Value | null>> = [];
  for (let i = 0; i < rowCount; i++) {
    const bitmap = r.raw(nb);
    const row: Array<Value | null> = [];
    for (let c = 0; c < columns.length; c++) {
      const isNull = ((bitmap[c >> 3] ?? 0) & (1 << (c & 7))) !== 0;
      row.push(isNull ? null : decodeUntagged(r, columns[c]!.typeTag));
    }
    rows.push(row);
  }
  return rows;
}

function decodeEntries(r: Reader): Array<[Buffer, Buffer]> {
  const count = r.u32();
  const entries: Array<[Buffer, Buffer]> = [];
  for (let i = 0; i < count; i++) entries.push([r.bytesU16(), r.bytesU32()]);
  return entries;
}

function decodeKvBody(op: number, r: Reader): KvResultBody {
  switch (op) {
    case 1: {
      const found = r.u8() !== 0;
      return { op: 1, value: found ? r.bytesU32() : null };
    }
    case 2:
      return { op: 2 };
    case 3:
      return { op: 3 };
    case 4:
      return { op: 4, entries: decodeEntries(r), moreFrames: r.u8() !== 0 };
    case 5:
      return { op: 5, entries: decodeEntries(r), moreFrames: r.u8() !== 0 };
    default:
      throw new ProtocolError(`unknown kv result op 0x${op.toString(16)}`);
  }
}
