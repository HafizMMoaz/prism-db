package dev.prism.client;

import java.util.Arrays;

/** A 12-byte document identifier. */
public final class ObjectId {
    private static final char[] HEX = "0123456789abcdef".toCharArray();

    private final byte[] bytes;

    public ObjectId(byte[] bytes) {
        if (bytes.length != 12) throw new ProtocolException("ObjectId must be 12 bytes");
        this.bytes = bytes.clone();
    }

    /** A defensive copy of the 12 bytes. */
    public byte[] bytes() {
        return bytes.clone();
    }

    /** Internal accessor that avoids the defensive copy on the encode path. */
    byte[] rawBytes() {
        return bytes;
    }

    /** Lowercase 24-character hex. */
    public String toHex() {
        char[] c = new char[24];
        for (int i = 0; i < 12; i++) {
            c[i * 2] = HEX[(bytes[i] >> 4) & 0xF];
            c[i * 2 + 1] = HEX[bytes[i] & 0xF];
        }
        return new String(c);
    }

    public static ObjectId fromHex(String hex) {
        if (hex.length() != 24) throw new ProtocolException("ObjectId hex must be 24 chars");
        byte[] b = new byte[12];
        for (int i = 0; i < 12; i++) {
            int hi = Character.digit(hex.charAt(i * 2), 16);
            int lo = Character.digit(hex.charAt(i * 2 + 1), 16);
            if (hi < 0 || lo < 0) throw new ProtocolException("ObjectId hex has a non-hex character");
            b[i] = (byte) ((hi << 4) | lo);
        }
        return new ObjectId(b);
    }

    @Override
    public String toString() {
        return toHex();
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof ObjectId && Arrays.equals(bytes, ((ObjectId) o).bytes);
    }

    @Override
    public int hashCode() {
        return Arrays.hashCode(bytes);
    }
}
