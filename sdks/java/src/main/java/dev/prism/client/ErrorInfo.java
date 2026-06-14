package dev.prism.client;

/** The structured error trailer a server attaches to a non-OK response. */
public final class ErrorInfo {
    /** A code from the wire spec's error-code ranges. */
    public final int code;
    /** A human-readable message. */
    public final String message;
    /** The 5-character SQLSTATE (e.g. {@code "23505"}). */
    public final String sqlstate;
    /** Optional extra detail (may be empty). */
    public final String detail;
    /** Character position in the source SQL, or 0. */
    public final int position;

    public ErrorInfo(int code, String message, String sqlstate, String detail, int position) {
        this.code = code;
        this.message = message;
        this.sqlstate = sqlstate;
        this.detail = detail;
        this.position = position;
    }
}
