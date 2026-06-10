// The transport: a TCP (optionally TLS) socket that frames outgoing messages,
// reassembles incoming frames, and matches each reply to its request by the
// echoed `request_id`. Server-initiated notices (request_id 0) go to a handler.

import net from "node:net";
import tls from "node:tls";

import { frameEncode, frameParse } from "./codec.js";
import { ProtocolError } from "./errors.js";
import { ClientMessage, ServerMessage, decodePacket, encodePacket } from "./messages.js";

/** A connection-level notice from the server (e.g. shutdown, idle timeout). */
export interface Notice {
  severity: number;
  code: number;
  message: string;
}

export interface ConnectionOptions {
  host?: string;
  port?: number;
  /** Use TLS (true for defaults, or explicit Node TLS options). */
  tls?: boolean | tls.ConnectionOptions;
  /** Connection timeout in milliseconds (default 10000). */
  connectTimeoutMs?: number;
  /** Called for unsolicited server notices. */
  onNotice?: (n: Notice) => void;
}

interface Pending {
  resolve: (m: ServerMessage) => void;
  reject: (e: Error) => void;
}

export class Connection {
  private inbound: Buffer = Buffer.alloc(0);
  private readonly pending = new Map<number, Pending>();
  private nextId = 1;
  private closedError: Error | null = null;

  private constructor(
    private readonly socket: net.Socket,
    private readonly onNotice?: (n: Notice) => void,
  ) {
    socket.on("data", (chunk) => this.onData(chunk));
    socket.on("error", (err) => this.fail(err));
    socket.on("close", () => this.fail(new ProtocolError("connection closed by server")));
  }

  /** Open a connection (TCP, or TLS when `opts.tls` is set). */
  static connect(opts: ConnectionOptions = {}): Promise<Connection> {
    const host = opts.host ?? "127.0.0.1";
    const port = opts.port ?? 4444;
    const timeout = opts.connectTimeoutMs ?? 10_000;

    return new Promise((resolve, reject) => {
      const onConnect = (socket: net.Socket) => {
        socket.setNoDelay(true);
        socket.setTimeout(0);
        resolve(new Connection(socket, opts.onNotice));
      };

      let socket: net.Socket;
      if (opts.tls) {
        const tlsOpts: tls.ConnectionOptions =
          typeof opts.tls === "object" ? opts.tls : {};
        socket = tls.connect({ host, port, ...tlsOpts }, () => onConnect(socket));
      } else {
        socket = net.connect({ host, port }, () => onConnect(socket));
      }

      socket.setTimeout(timeout, () => {
        socket.destroy(new ProtocolError(`connect timed out after ${timeout}ms`));
      });
      socket.once("error", reject);
    });
  }

  /** Send a client message and resolve with the matching reply. */
  send(message: ClientMessage): Promise<ServerMessage> {
    if (this.closedError) return Promise.reject(this.closedError);
    const requestId = this.nextId;
    this.nextId = this.nextId >= 0xffffffff ? 1 : this.nextId + 1;

    return new Promise<ServerMessage>((resolve, reject) => {
      this.pending.set(requestId, { resolve, reject });
      const frame = frameEncode(encodePacket(requestId, message));
      this.socket.write(frame, (err) => {
        if (err) {
          this.pending.delete(requestId);
          reject(err);
        }
      });
    });
  }

  /** Close the connection. Pending requests are rejected. */
  close(): void {
    this.fail(new ProtocolError("connection closed by client"));
    this.socket.end();
  }

  private onData(chunk: Buffer): void {
    this.inbound = this.inbound.length === 0 ? chunk : Buffer.concat([this.inbound, chunk]);
    for (;;) {
      const parsed = frameParse(this.inbound);
      if (!parsed) break;
      this.inbound = this.inbound.subarray(parsed.consumed);
      let packet;
      try {
        packet = decodePacket(parsed.payload);
      } catch (err) {
        this.fail(err as Error);
        return;
      }
      if (packet.message.type === "notice") {
        this.onNotice?.({
          severity: packet.message.severity,
          code: packet.message.code,
          message: packet.message.message,
        });
        continue;
      }
      const waiter = this.pending.get(packet.requestId);
      if (waiter) {
        this.pending.delete(packet.requestId);
        waiter.resolve(packet.message);
      }
      // An unmatched reply (e.g. a late response after timeout) is ignored.
    }
  }

  private fail(err: Error): void {
    if (this.closedError) return;
    this.closedError = err;
    for (const waiter of this.pending.values()) waiter.reject(err);
    this.pending.clear();
  }
}
