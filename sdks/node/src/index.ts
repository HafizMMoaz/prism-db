// @prismdb/client — a pure-TypeScript client for PrismDB over the binary wire
// protocol (docs/specs/wire-protocol.md). No native build.

export { Client } from "./client.js";
export type { ConnectOptions, SqlResult } from "./client.js";
export type { ConnectionOptions, Notice } from "./connection.js";
export { Q } from "./query.js";
export type { DocQuery } from "./query.js";
export { U } from "./update.js";
export type { DocUpdate } from "./update.js";
export type { Document } from "./document.js";
export { ObjectId, Typed, int32, int64, float64, timestamp, TAG } from "./value.js";
export type { Value } from "./value.js";
export type { ColumnDesc } from "./messages.js";
export { PrismServerError, ProtocolError } from "./errors.js";
export type { ErrorInfo } from "./errors.js";
