package dev.prism.client;

import java.nio.charset.StandardCharsets;
import java.util.Arrays;

/**
 * Low-level binary codec: a growable little-endian {@link Writer}, a
 * bounds-checked {@link Reader}, and the length-prefixed {@link Frame} helper.
 * The byte layouts mirror {@code crates/prism-protocol/src/codec.rs} exactly
 * (all multi-byte integers little-endian).
 */
final class Codec {
    private Codec() {}
}

/** A growable little-endian writer over a byte buffer. */
final class Writer {
    private byte[] buf = new byte[64];
    private int len = 0;

    private void ensure(int extra) {
        int needed = len + extra;
        if (needed <= buf.length) return;
        int cap = buf.length * 2;
        while (cap < needed) cap *= 2;
        buf = Arrays.copyOf(buf, cap);
    }

    void u8(int v) {
        ensure(1);
        buf[len++] = (byte) v;
    }

    void u16(int v) {
        ensure(2);
        buf[len++] = (byte) v;
        buf[len++] = (byte) (v >>> 8);
    }

    void u32(long v) {
        ensure(4);
        for (int i = 0; i < 4; i++) buf[len++] = (byte) (v >>> (8 * i));
    }

    void i32(int v) {
        ensure(4);
        for (int i = 0; i < 4; i++) buf[len++] = (byte) (v >>> (8 * i));
    }

    void u64(long v) {
        ensure(8);
        for (int i = 0; i < 8; i++) buf[len++] = (byte) (v >>> (8 * i));
    }

    void i64(long v) {
        u64(v);
    }

    void f64(double d) {
        u64(Double.doubleToLongBits(d));
    }

    /** A 128-bit unsigned integer as 16 little-endian bytes (low 64 bits = {@code lo}, high = 0). */
    void u128(long lo) {
        u64(lo);
        u64(0);
    }

    void raw(byte[] b) {
        ensure(b.length);
        System.arraycopy(b, 0, buf, len, b.length);
        len += b.length;
    }

    /** A UTF-8 string with a u16 length prefix. */
    void strU16(String s) {
        byte[] b = s.getBytes(StandardCharsets.UTF_8);
        u16(b.length);
        raw(b);
    }

    /** A UTF-8 string with a u32 length prefix. */
    void strU32(String s) {
        byte[] b = s.getBytes(StandardCharsets.UTF_8);
        u32(b.length);
        raw(b);
    }

    /** A byte string with a u16 length prefix. */
    void bytesU16(byte[] b) {
        u16(b.length);
        raw(b);
    }

    /** A byte string with a u32 length prefix. */
    void bytesU32(byte[] b) {
        u32(b.length);
        raw(b);
    }

    byte[] out() {
        return Arrays.copyOf(buf, len);
    }
}

/** A bounds-checked little-endian reader over a byte array. */
final class Reader {
    private final byte[] buf;
    private int p = 0;

    Reader(byte[] buf) {
        this.buf = buf;
    }

    private void need(int n) {
        if (p + n > buf.length) {
            throw new ProtocolException("truncated: need " + n + " bytes at offset " + p);
        }
    }

    int u8() {
        need(1);
        return buf[p++] & 0xFF;
    }

    int u16() {
        need(2);
        int v = (buf[p] & 0xFF) | ((buf[p + 1] & 0xFF) << 8);
        p += 2;
        return v;
    }

    long u32() {
        need(4);
        long v = 0;
        for (int i = 0; i < 4; i++) v |= ((long) (buf[p + i] & 0xFF)) << (8 * i);
        p += 4;
        return v;
    }

    int i32() {
        need(4);
        int v = 0;
        for (int i = 0; i < 4; i++) v |= (buf[p + i] & 0xFF) << (8 * i);
        p += 4;
        return v;
    }

    long u64() {
        need(8);
        long v = 0;
        for (int i = 0; i < 8; i++) v |= ((long) (buf[p + i] & 0xFF)) << (8 * i);
        p += 8;
        return v;
    }

    long i64() {
        return u64();
    }

    double f64() {
        return Double.longBitsToDouble(u64());
    }

    void skip(int n) {
        need(n);
        p += n;
    }

    byte[] raw(int n) {
        need(n);
        byte[] s = Arrays.copyOfRange(buf, p, p + n);
        p += n;
        return s;
    }

    String strU16() {
        return new String(raw(u16()), StandardCharsets.UTF_8);
    }

    String strU32() {
        return new String(raw((int) u32()), StandardCharsets.UTF_8);
    }

    byte[] bytesU16() {
        return raw(u16());
    }

    byte[] bytesU32() {
        return raw((int) u32());
    }

    int remaining() {
        return buf.length - p;
    }

    /** Throw unless every byte has been consumed. */
    void expectEnd() {
        if (remaining() != 0) {
            throw new ProtocolException(remaining() + " trailing byte(s) after message");
        }
    }
}

/** Length-prefixed framing: {@code [len:u32][payload]}. */
final class Frame {
    private Frame() {}

    static byte[] encode(byte[] payload) {
        byte[] out = new byte[4 + payload.length];
        long n = payload.length;
        for (int i = 0; i < 4; i++) out[i] = (byte) (n >>> (8 * i));
        System.arraycopy(payload, 0, out, 4, payload.length);
        return out;
    }
}
