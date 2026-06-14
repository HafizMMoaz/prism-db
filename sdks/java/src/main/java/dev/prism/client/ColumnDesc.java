package dev.prism.client;

/** A SQL result column descriptor. */
public final class ColumnDesc {
    public final String name;
    /** The wire type tag (see {@link Tag}). */
    public final int typeTag;
    public final boolean nullable;

    ColumnDesc(String name, int typeTag, boolean nullable) {
        this.name = name;
        this.typeTag = typeTag;
        this.nullable = nullable;
    }
}
