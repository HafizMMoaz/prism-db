package dev.prism.client;

import java.util.List;
import java.util.Map;

/** A SQL result set. {@code rows} are keyed by column name; {@code raw} keeps cell order. */
public final class SqlResult {
    public final List<ColumnDesc> columns;
    /** Rows as maps keyed by column name (insertion-ordered). */
    public final List<Map<String, Object>> rows;
    /** Rows as arrays of cells in column order. */
    public final List<Object[]> raw;
    public final long affectedRows;

    SqlResult(List<ColumnDesc> columns, List<Map<String, Object>> rows, List<Object[]> raw, long affectedRows) {
        this.columns = columns;
        this.rows = rows;
        this.raw = raw;
        this.affectedRows = affectedRows;
    }
}
