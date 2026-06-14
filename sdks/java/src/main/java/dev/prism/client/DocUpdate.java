package dev.prism.client;

import java.util.List;

/**
 * One document field mutation. Construct via the {@link U} helpers; encode a
 * list with {@link UpdateCodec}. Mirrors {@code prism_protocol::DocUpdate}.
 */
public final class DocUpdate {
    final String op;     // "set" | "unset" | "inc"
    final String field;
    final Object value;
    final long delta;

    DocUpdate(String op, String field, Object value, long delta) {
        this.op = op;
        this.field = field;
        this.value = value;
        this.delta = delta;
    }
}

/** Encodes an ordered list of updates to the {@code update} blob of a command. */
final class UpdateCodec {
    private UpdateCodec() {}

    static byte[] encode(List<DocUpdate> ops) {
        Writer w = new Writer();
        w.u32(ops.size());
        for (DocUpdate op : ops) {
            switch (op.op) {
                case "set":
                    w.u8(1);
                    w.strU16(op.field);
                    ValueCodec.encodeTagged(w, op.value);
                    break;
                case "unset":
                    w.u8(2);
                    w.strU16(op.field);
                    break;
                case "inc":
                    w.u8(3);
                    w.strU16(op.field);
                    w.i64(op.delta);
                    break;
                default:
                    throw new ProtocolException("unknown update op: " + op.op);
            }
        }
        return w.out();
    }
}
