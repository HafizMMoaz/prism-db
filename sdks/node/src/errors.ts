// Error types surfaced by the SDK.

/** A malformed frame/message, or a byte-level decode failure. */
export class ProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ProtocolError";
  }
}

/** The structured error trailer a server attaches to a non-OK response. */
export interface ErrorInfo {
  /** A code from the wire spec's error-code ranges. */
  code: number;
  /** A human-readable message. */
  message: string;
  /** The 5-character SQLSTATE (e.g. "23505"). */
  sqlstate: string;
  /** Optional extra detail (may be empty). */
  detail: string;
  /** Character position in the source SQL, or 0. */
  position: number;
}

/** An error returned by the server (status != 0), carrying its trailer. */
export class PrismServerError extends Error {
  readonly code: number;
  readonly sqlstate: string;
  readonly detail: string;
  readonly position: number;

  constructor(info: ErrorInfo) {
    super(info.message || `server error 0x${info.code.toString(16)}`);
    this.name = "PrismServerError";
    this.code = info.code;
    this.sqlstate = info.sqlstate;
    this.detail = info.detail;
    this.position = info.position;
  }
}
