// Document update operators and their wire encoding.
//
// Mirrors `prism_protocol::DocUpdate`: an ordered list of $set / $unset / $inc
// mutations. Build with the `U` helpers. Operand values reuse the tagged
// `Value` encoding. Carried as the `update` blob of an update command.

import { Writer } from "./codec.js";
import { Value, encodeTagged } from "./value.js";

/** One field mutation. Construct via the {@link U} helpers. */
export type DocUpdate =
  | { op: "set"; field: string; value: Value }
  | { op: "unset"; field: string }
  | { op: "inc"; field: string; delta: number | bigint };

/** Update builders mirroring the engine's update operators. */
export const U = {
  /** `$set` - set `field` to `value`. */
  set(field: string, value: Value): DocUpdate {
    return { op: "set", field, value };
  },
  /** `$unset` - remove `field`. */
  unset(field: string): DocUpdate {
    return { op: "unset", field };
  },
  /** `$inc` - add `delta` to the integer `field`. */
  inc(field: string, delta: number | bigint): DocUpdate {
    return { op: "inc", field, delta };
  },
} as const;

/** Encode a list of update operations to the `update` blob of a command. */
export function encodeDocUpdate(ops: DocUpdate[]): Buffer {
  const w = new Writer();
  w.u32(ops.length);
  for (const op of ops) {
    switch (op.op) {
      case "set":
        w.u8(1);
        w.strU16(op.field);
        encodeTagged(w, op.value);
        break;
      case "unset":
        w.u8(2);
        w.strU16(op.field);
        break;
      case "inc":
        w.u8(3);
        w.strU16(op.field);
        w.i64(BigInt(op.delta));
        break;
    }
  }
  return Buffer.from(w.out());
}
