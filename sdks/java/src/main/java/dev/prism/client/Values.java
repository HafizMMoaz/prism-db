package dev.prism.client;

/**
 * Helpers to build explicitly-typed values. By default Java integer types
 * ({@code Integer}, {@code Long}, ...) map to wire Int64 to match the reference
 * SDK; use {@link #int32} to force Int32.
 */
public final class Values {
    private Values() {}

    /** Force a value to wire Int32. */
    public static Typed int32(int n) {
        return new Typed(Tag.INT32, n);
    }

    /** Force a value to wire Int64. */
    public static Typed int64(long n) {
        return new Typed(Tag.INT64, n);
    }

    /** Force a value to wire Double. */
    public static Typed float64(double n) {
        return new Typed(Tag.DOUBLE, n);
    }

    /** Force a value to wire Timestamp (microseconds since the Unix epoch). */
    public static Typed timestamp(long micros) {
        return new Typed(Tag.TIMESTAMP, micros);
    }
}
