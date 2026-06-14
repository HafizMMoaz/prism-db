package dev.prism.client;

import java.util.LinkedHashMap;
import java.util.Map;

/**
 * A document: an insertion-ordered string-keyed map of values. Field values map
 * with the same rules as SQL parameters (see {@link ValueCodec}), except that
 * documents do not support Binary values.
 */
public final class Document extends LinkedHashMap<String, Object> {
    public Document() {
        super();
    }

    public Document(Map<String, Object> initial) {
        super(initial);
    }

    /** Fluent setter: {@code new Document().set("a", 1).set("b", "x")}. */
    public Document set(String key, Object value) {
        put(key, value);
        return this;
    }
}
