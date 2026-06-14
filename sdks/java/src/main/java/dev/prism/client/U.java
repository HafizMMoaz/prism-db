package dev.prism.client;

/** Update builders mirroring the engine's update operators. */
public final class U {
    private U() {}

    /** {@code $set} — set {@code field} to {@code value}. */
    public static DocUpdate set(String field, Object value) {
        return new DocUpdate("set", field, value, 0);
    }

    /** {@code $unset} — remove {@code field}. */
    public static DocUpdate unset(String field) {
        return new DocUpdate("unset", field, null, 0);
    }

    /** {@code $inc} — add {@code delta} to the integer {@code field}. */
    public static DocUpdate inc(String field, long delta) {
        return new DocUpdate("inc", field, null, delta);
    }
}
