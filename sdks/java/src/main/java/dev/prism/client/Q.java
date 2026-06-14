package dev.prism.client;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/** Query builders mirroring the engine's filter set. */
public final class Q {
    private Q() {}

    /** Match every document. */
    public static DocQuery all() {
        return new DocQuery("all");
    }

    public static DocQuery eq(String field, Object value) {
        return field(DocQuery.T_EQ, field, value);
    }

    public static DocQuery ne(String field, Object value) {
        return field(DocQuery.T_NE, field, value);
    }

    public static DocQuery gt(String field, Object value) {
        return field(DocQuery.T_GT, field, value);
    }

    public static DocQuery lt(String field, Object value) {
        return field(DocQuery.T_LT, field, value);
    }

    public static DocQuery gte(String field, Object value) {
        return field(DocQuery.T_GTE, field, value);
    }

    public static DocQuery lte(String field, Object value) {
        return field(DocQuery.T_LTE, field, value);
    }

    public static DocQuery in(String field, List<?> values) {
        return set(DocQuery.T_IN, field, values);
    }

    public static DocQuery nin(String field, List<?> values) {
        return set(DocQuery.T_NIN, field, values);
    }

    public static DocQuery exists(String field) {
        return exists(field, true);
    }

    public static DocQuery exists(String field, boolean present) {
        DocQuery q = new DocQuery("exists");
        q.field = field;
        q.present = present;
        return q;
    }

    public static DocQuery and(DocQuery... subs) {
        return group(DocQuery.T_AND, subs);
    }

    public static DocQuery or(DocQuery... subs) {
        return group(DocQuery.T_OR, subs);
    }

    public static DocQuery not(DocQuery sub) {
        DocQuery q = new DocQuery("not");
        q.sub = sub;
        return q;
    }

    private static DocQuery field(int tag, String field, Object value) {
        DocQuery q = new DocQuery("field");
        q.tag = tag;
        q.field = field;
        q.value = value;
        return q;
    }

    private static DocQuery set(int tag, String field, List<?> values) {
        DocQuery q = new DocQuery("set");
        q.tag = tag;
        q.field = field;
        q.values = new ArrayList<>(values);
        return q;
    }

    private static DocQuery group(int tag, DocQuery[] subs) {
        DocQuery q = new DocQuery("group");
        q.tag = tag;
        q.subs = new ArrayList<>(Arrays.asList(subs));
        return q;
    }
}
