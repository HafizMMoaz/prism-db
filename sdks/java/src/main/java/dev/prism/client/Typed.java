package dev.prism.client;

/**
 * An explicitly-typed value, for cases where the default mapping of a Java value
 * is not what you want (e.g. a 32-bit int, a float that happens to be integral,
 * or a timestamp). Build with the {@link Values} helpers.
 */
public final class Typed {
    final int tag;
    final Object value;

    Typed(int tag, Object value) {
        this.tag = tag;
        this.value = value;
    }
}
