package dev.prism.client;

import java.time.Instant;

/**
 * The scalar value tagged/untagged wire codec. Plain Java values map as:
 * {@code null}->Null, {@code Boolean}->Bool, integer boxes->Int64,
 * {@code Float}/{@code Double}->Double, {@code String}->String,
 * {@code byte[]}->Binary, {@code Instant}->Timestamp, {@link ObjectId}->ObjectId.
 * Wrap with {@link Values} to force the other wire types.
 */
final class ValueCodec {
    private ValueCodec() {}

    static int tagOf(Object v) {
        if (v == null) return Tag.NULL;
        if (v instanceof Typed) return ((Typed) v).tag;
        if (v instanceof ObjectId) return Tag.OBJECTID;
        if (v instanceof Boolean) return Tag.BOOL;
        if (v instanceof Byte || v instanceof Short || v instanceof Integer || v instanceof Long) return Tag.INT64;
        if (v instanceof Float || v instanceof Double) return Tag.DOUBLE;
        if (v instanceof String) return Tag.STRING;
        if (v instanceof byte[]) return Tag.BINARY;
        if (v instanceof Instant) return Tag.TIMESTAMP;
        throw new ProtocolException("unsupported value: " + v.getClass().getName());
    }

    static void encodeUntagged(Writer w, int tag, Object v) {
        Object raw = v instanceof Typed ? ((Typed) v).value : v;
        switch (tag) {
            case Tag.NULL:
                break;
            case Tag.BOOL:
                w.u8(((Boolean) raw) ? 1 : 0);
                break;
            case Tag.INT32:
                w.i32(((Number) raw).intValue());
                break;
            case Tag.INT64:
                w.i64(((Number) raw).longValue());
                break;
            case Tag.DOUBLE:
                w.f64(((Number) raw).doubleValue());
                break;
            case Tag.TIMESTAMP:
                w.i64(raw instanceof Instant ? epochMicros((Instant) raw) : ((Number) raw).longValue());
                break;
            case Tag.STRING:
                w.strU32((String) raw);
                break;
            case Tag.OBJECTID:
                w.raw(((ObjectId) raw).rawBytes());
                break;
            case Tag.BINARY: {
                byte[] b = (byte[]) raw;
                w.u32(b.length);
                w.u8(0); // subtype
                w.raw(b);
                break;
            }
            default:
                throw new ProtocolException(String.format("cannot encode value tag 0x%x", tag));
        }
    }

    static void encodeTagged(Writer w, Object v) {
        int tag = tagOf(v);
        w.u8(tag);
        encodeUntagged(w, tag, v);
    }

    static Object decodeUntagged(Reader r, int tag) {
        switch (tag) {
            case Tag.NULL:
                return null;
            case Tag.BOOL:
                return r.u8() != 0;
            case Tag.INT32:
                return r.i32();
            case Tag.INT64:
                return r.i64();
            case Tag.DOUBLE:
                return r.f64();
            case Tag.TIMESTAMP:
                return r.i64();
            case Tag.STRING:
                return r.strU32();
            case Tag.OBJECTID:
                return new ObjectId(r.raw(12));
            case Tag.BINARY: {
                int len = (int) r.u32();
                r.u8(); // subtype (discarded)
                return r.raw(len);
            }
            default:
                throw new ProtocolException(String.format("unknown value tag 0x%x", tag));
        }
    }

    static Object decodeTagged(Reader r) {
        return decodeUntagged(r, r.u8());
    }

    private static long epochMicros(Instant i) {
        return i.getEpochSecond() * 1_000_000L + i.getNano() / 1000L;
    }
}
