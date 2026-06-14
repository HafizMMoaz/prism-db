package dev.prism.client;

import java.util.List;

/**
 * A document query filter. Construct via the {@link Q} helpers; encode it with
 * {@link QueryCodec}. Mirrors {@code prism_protocol::DocQuery}: a tag byte, then
 * the operator-specific body.
 */
public final class DocQuery {
    // discriminant tags, identical to the Rust DocQuery::encode
    static final int T_ALL = 0, T_EQ = 1, T_NE = 2, T_GT = 3, T_LT = 4, T_GTE = 5, T_LTE = 6,
            T_IN = 7, T_NIN = 8, T_EXISTS = 9, T_AND = 10, T_OR = 11, T_NOT = 12;

    final String kind;
    int tag;
    String field;
    Object value;
    List<Object> values;
    boolean present = true;
    List<DocQuery> subs;
    DocQuery sub;

    DocQuery(String kind) {
        this.kind = kind;
    }
}

/** Encodes a {@link DocQuery} to the standalone bytes carried in a document command. */
final class QueryCodec {
    private QueryCodec() {}

    static byte[] encode(DocQuery q) {
        Writer w = new Writer();
        encodeInto(w, q);
        return w.out();
    }

    static void encodeInto(Writer w, DocQuery q) {
        switch (q.kind) {
            case "all":
                w.u8(DocQuery.T_ALL);
                break;
            case "field":
                w.u8(q.tag);
                w.strU16(q.field);
                ValueCodec.encodeTagged(w, q.value);
                break;
            case "set":
                w.u8(q.tag);
                w.strU16(q.field);
                w.u32(q.values.size());
                for (Object v : q.values) ValueCodec.encodeTagged(w, v);
                break;
            case "exists":
                w.u8(DocQuery.T_EXISTS);
                w.strU16(q.field);
                w.u8(q.present ? 1 : 0);
                break;
            case "group":
                w.u8(q.tag);
                w.u32(q.subs.size());
                for (DocQuery s : q.subs) encodeInto(w, s);
                break;
            case "not":
                w.u8(DocQuery.T_NOT);
                encodeInto(w, q.sub);
                break;
            default:
                throw new ProtocolException("unknown query kind: " + q.kind);
        }
    }
}
