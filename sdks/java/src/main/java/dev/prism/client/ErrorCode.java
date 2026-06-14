package dev.prism.client;

/** Stable error codes from {@code docs/specs/wire-protocol.md}. */
public final class ErrorCode {
    private ErrorCode() {}

    public static final int PROTOCOL_VIOLATION = 0x0001;
    public static final int CONNECTION_CLOSED = 0x0002;
    public static final int AUTHENTICATION_FAILED = 0x0101;
    public static final int UNAUTHORIZED = 0x0102;
    public static final int SERIALIZATION_FAILURE = 0x0201;
    public static final int DEADLOCK = 0x0202;
    public static final int TRANSACTION_TIMEOUT = 0x0203;
    public static final int TRANSACTION_ABORTED = 0x0204;
    public static final int IO_ERROR = 0x0301;
    public static final int OUT_OF_DISK_SPACE = 0x0302;
    public static final int SYNTAX_ERROR = 0x0401;
    public static final int TYPE_ERROR = 0x0402;
    public static final int OBJECT_NOT_FOUND = 0x0403;
    public static final int OBJECT_ALREADY_EXISTS = 0x0404;
    public static final int UNIQUE_VIOLATION = 0x0501;
    public static final int CHECK_VIOLATION = 0x0502;
    public static final int OUT_OF_MEMORY = 0x0601;
    public static final int TOO_MANY_CONNECTIONS = 0x0602;
    public static final int QUERY_TOO_COMPLEX = 0x0603;
    public static final int INTERNAL_ERROR = 0xFF01;
}
