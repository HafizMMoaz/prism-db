package dev.prism.client;

/** Record-format type tags (see {@code docs/specs/record-format.md}). */
public final class Tag {
    private Tag() {}

    public static final int NULL = 0x00;
    public static final int BOOL = 0x01;
    public static final int INT32 = 0x02;
    public static final int INT64 = 0x03;
    public static final int DOUBLE = 0x04;
    public static final int STRING = 0x05;
    public static final int BINARY = 0x06;
    public static final int TIMESTAMP = 0x09;
    public static final int OBJECTID = 0x0A;
}
