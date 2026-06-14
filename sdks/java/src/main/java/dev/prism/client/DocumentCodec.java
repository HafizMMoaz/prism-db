package dev.prism.client;

import java.util.Map;

/**
 * The document tagged-binary codec.
 *
 * Mirrors {@code crates/prism-doc/src/value.rs}. A document is
 * {@code [total:u32][count:u16]} followed by, per field,
 * {@code [tag:u8][nameLen:u16][name][value bytes]}. Field value bytes use the
 * same encoding as scalar values, except documents have no Binary type.
 */
final class DocumentCodec {
    private DocumentCodec() {}

    static byte[] encode(Map<String, Object> doc) {
        if (doc.size() > 0xFFFF) throw new ProtocolException("too many document fields");
        Writer body = new Writer();
        body.u16(doc.size());
        for (Map.Entry<String, Object> e : doc.entrySet()) {
            int tag = ValueCodec.tagOf(e.getValue());
            if (tag == Tag.BINARY) {
                throw new ProtocolException("field \"" + e.getKey() + "\": binary values are not supported in documents");
            }
            body.u8(tag);
            body.strU16(e.getKey());
            ValueCodec.encodeUntagged(body, tag, e.getValue());
        }
        byte[] inner = body.out();
        Writer out = new Writer();
        out.u32(4L + inner.length); // total length, including this u32
        out.raw(inner);
        return out.out();
    }

    static Document decode(byte[] bytes) {
        Reader r = new Reader(bytes);
        r.u32(); // total length (redundant with the frame's blob length)
        int count = r.u16();
        Document d = new Document();
        for (int i = 0; i < count; i++) {
            int tag = r.u8();
            String name = r.strU16();
            d.put(name, ValueCodec.decodeUntagged(r, tag));
        }
        return d;
    }
}
