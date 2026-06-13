// The high-level client: connect + handshake, then SQL / KV / document calls
// and transaction control. One client owns one connection = one server session,
// so a `begin()`…`commit()` brackets the calls in between.

import { Connection, ConnectionOptions } from "./connection.js";
import { Document, decodeDocument, encodeDocument } from "./document.js";
import { ErrorInfo, PrismServerError, ProtocolError } from "./errors.js";
import {
  AUTH_PASSWORD,
  ColumnDesc,
  ServerMessage,
  TXN_READ_ONLY,
  TXN_READ_WRITE,
} from "./messages.js";
import { DocQuery, Q, encodeDocQuery } from "./query.js";
import { DocUpdate, encodeDocUpdate } from "./update.js";
import { ObjectId, Value } from "./value.js";

const PROTOCOL_VERSION = 1;
const EMPTY = Buffer.alloc(0);

/** Options for {@link Client.connect}. */
export interface ConnectOptions extends ConnectionOptions {
  username?: string;
  password?: string;
  clientName?: string;
  clientVersion?: string;
}

/** A SQL result set. `rows` are keyed by column name; `raw` keeps cell order. */
export interface SqlResult {
  columns: ColumnDesc[];
  rows: Array<Record<string, Value | null>>;
  raw: Array<Array<Value | null>>;
  affectedRows: bigint;
}

function fail(error: ErrorInfo | undefined): never {
  throw new PrismServerError(error ?? { code: 0, message: "server error", sqlstate: "XX000", detail: "", position: 0 });
}

export class Client {
  private constructor(private readonly conn: Connection) {}

  /** Connect, perform the handshake, and (if `username` is set) authenticate. */
  static async connect(opts: ConnectOptions = {}): Promise<Client> {
    const conn = await Connection.connect(opts);
    const client = new Client(conn);
    try {
      await client.handshake(opts);
    } catch (err) {
      conn.close();
      throw err;
    }
    return client;
  }

  private async handshake(opts: ConnectOptions): Promise<void> {
    const helloAck = await this.conn.send({
      type: "hello",
      protocolVersion: PROTOCOL_VERSION,
      clientName: opts.clientName ?? "@prismdb/client",
      clientVersion: opts.clientVersion ?? "0.1.0",
      features: 0,
    });
    if (helloAck.type !== "helloAck") throw new ProtocolError("expected HelloAck");
    if (helloAck.status !== 0) fail(helloAck.error);

    if (opts.username !== undefined) {
      const authAck = await this.conn.send({
        type: "auth",
        mechanism: AUTH_PASSWORD,
        username: opts.username,
        password: opts.password ?? "",
      });
      if (authAck.type !== "authAck") throw new ProtocolError("expected AuthAck");
      if (authAck.status !== 0) fail(authAck.error);
    }
  }

  // ---- SQL ------------------------------------------------------------------

  /** Execute a SQL statement. Returns rows for `SELECT`, counts otherwise. */
  async sql(
    text: string,
    opts: { params?: Value[]; returnRows?: boolean } = {},
  ): Promise<SqlResult> {
    const reply = await this.conn.send({
      type: "sqlExecute",
      sql: text,
      params: opts.params ?? [],
      options: (opts.returnRows ?? true) ? 1 : 0,
    });
    if (reply.type !== "sqlResult") throw new ProtocolError("expected SqlResult");
    if (reply.status !== 0) fail(reply.error);
    if (reply.moreFrames) throw new ProtocolError("streamed SQL results are not yet supported");

    const names = reply.columns.map((c) => c.name);
    const rows = reply.rows.map((cells) => {
      const obj: Record<string, Value | null> = {};
      names.forEach((name, i) => {
        obj[name] = cells[i] ?? null;
      });
      return obj;
    });
    return { columns: reply.columns, rows, raw: reply.rows, affectedRows: reply.affectedRows };
  }

  // ---- transactions ---------------------------------------------------------

  /** Begin a transaction; returns the assigned transaction id. */
  async begin(mode: "readWrite" | "readOnly" = "readWrite"): Promise<bigint> {
    const ack = await this.txn({ type: "begin", mode: mode === "readOnly" ? TXN_READ_ONLY : TXN_READ_WRITE });
    return ack.txnId;
  }

  /** Commit the current transaction (optionally idempotent). */
  async commit(opts: { idempotencyKey?: bigint } = {}): Promise<void> {
    await this.txn({ type: "commit", idempotencyKey: opts.idempotencyKey ?? 0n });
  }

  /** Abort the current transaction. */
  async abort(): Promise<void> {
    await this.txn({ type: "abort" });
  }

  private async txn(
    msg: { type: "begin"; mode: number } | { type: "commit"; idempotencyKey: bigint } | { type: "abort" },
  ): Promise<{ txnId: bigint; commitLsn: bigint }> {
    const reply = await this.conn.send(msg);
    if (reply.type !== "txnAck") throw new ProtocolError("expected TxnAck");
    if (reply.status !== 0) fail(reply.error);
    return { txnId: reply.txnId, commitLsn: reply.commitLsn };
  }

  // ---- KV -------------------------------------------------------------------

  readonly kv = {
    get: async (namespace: string, key: BytesLike): Promise<Buffer | null> => {
      const reply = await this.kvReply({ type: "kvOp", namespace, command: { op: 1, key: bytes(key) } });
      if (reply.body.op !== 1) throw new ProtocolError("expected a KV get result");
      return reply.body.value;
    },
    put: async (namespace: string, key: BytesLike, value: BytesLike): Promise<void> => {
      await this.kvReply({ type: "kvOp", namespace, command: { op: 2, key: bytes(key), value: bytes(value) } });
    },
    delete: async (namespace: string, key: BytesLike): Promise<void> => {
      await this.kvReply({ type: "kvOp", namespace, command: { op: 3, key: bytes(key) } });
    },
  };

  private async kvReply(msg: Parameters<Connection["send"]>[0]) {
    const reply = await this.conn.send(msg);
    if (reply.type !== "kvResult") throw new ProtocolError("expected KvResult");
    if (reply.status !== 0) fail(reply.error);
    return reply;
  }

  // ---- documents ------------------------------------------------------------

  readonly doc = {
    insertOne: async (collection: string, document: Document): Promise<ObjectId> => {
      const reply = await this.docReply({
        type: "docOp",
        collection,
        command: { op: 1, doc: encodeDocument(document) },
      });
      const id = reply.insertedIds[0];
      if (!id) throw new ProtocolError("insert returned no _id");
      return id;
    },

    insertMany: async (collection: string, documents: Document[]): Promise<ObjectId[]> => {
      const reply = await this.docReply({
        type: "docOp",
        collection,
        command: { op: 2, docs: documents.map(encodeDocument) },
      });
      return reply.insertedIds;
    },

    find: async (collection: string, query: DocQuery = Q.all()): Promise<Document[]> => {
      const reply = await this.docReply({
        type: "docOp",
        collection,
        command: { op: 3, query: encodeDocQuery(query), options: EMPTY },
      });
      return reply.docs.map(decodeDocument);
    },

    findOne: async (collection: string, query: DocQuery = Q.all()): Promise<Document | null> => {
      const reply = await this.docReply({
        type: "docOp",
        collection,
        command: { op: 4, query: encodeDocQuery(query), options: EMPTY },
      });
      const first = reply.docs[0];
      return first ? decodeDocument(first) : null;
    },

    count: async (collection: string, query: DocQuery = Q.all()): Promise<bigint> => {
      const reply = await this.docReply({
        type: "docOp",
        collection,
        command: { op: 9, query: encodeDocQuery(query), options: EMPTY },
      });
      return reply.affected;
    },

    updateOne: async (collection: string, query: DocQuery, update: DocUpdate[]): Promise<bigint> => {
      const reply = await this.docReply({
        type: "docOp",
        collection,
        command: { op: 5, query: encodeDocQuery(query), update: encodeDocUpdate(update), options: EMPTY },
      });
      return reply.affected;
    },

    updateMany: async (collection: string, query: DocQuery, update: DocUpdate[]): Promise<bigint> => {
      const reply = await this.docReply({
        type: "docOp",
        collection,
        command: { op: 6, query: encodeDocQuery(query), update: encodeDocUpdate(update), options: EMPTY },
      });
      return reply.affected;
    },

    deleteOne: async (collection: string, query: DocQuery): Promise<bigint> => {
      const reply = await this.docReply({
        type: "docOp",
        collection,
        command: { op: 7, query: encodeDocQuery(query), options: EMPTY },
      });
      return reply.affected;
    },

    deleteMany: async (collection: string, query: DocQuery): Promise<bigint> => {
      const reply = await this.docReply({
        type: "docOp",
        collection,
        command: { op: 8, query: encodeDocQuery(query), options: EMPTY },
      });
      return reply.affected;
    },
  };

  private async docReply(msg: Parameters<Connection["send"]>[0]) {
    const reply = await this.conn.send(msg);
    if (reply.type !== "docResult") throw new ProtocolError("expected DocResult");
    if (reply.status !== 0) fail(reply.error);
    if (reply.moreFrames) throw new ProtocolError("streamed document results are not yet supported");
    return reply;
  }

  // ---- misc -----------------------------------------------------------------

  /** Round-trip a keep-alive ping. */
  async ping(): Promise<void> {
    const reply = await this.conn.send({ type: "ping" });
    if (reply.type !== "pong") throw new ProtocolError("expected Pong");
  }

  /** Close the underlying connection. */
  close(): void {
    this.conn.close();
  }
}

type BytesLike = string | Uint8Array | Buffer;

function bytes(v: BytesLike): Buffer {
  return typeof v === "string" ? Buffer.from(v, "utf8") : Buffer.from(v);
}
