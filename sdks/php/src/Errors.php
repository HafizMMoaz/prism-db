<?php

declare(strict_types=1);

namespace PrismDb;

/** Base class for every error raised by this SDK. */
abstract class PrismException extends \RuntimeException
{
}

/** A malformed frame/message, or a byte-level decode failure. */
final class ProtocolException extends PrismException
{
}

/** The structured error trailer a server attaches to a non-OK response. */
final class ErrorInfo
{
    public function __construct(
        /** A code from the wire spec's error-code ranges. */
        public int $code,
        /** A human-readable message. */
        public string $message,
        /** The 5-character SQLSTATE (e.g. "23505"). */
        public string $sqlstate,
        /** Optional extra detail (may be empty). */
        public string $detail = '',
        /** Character position in the source SQL, or 0. */
        public int $position = 0,
    ) {
    }
}

/** An error returned by the server (status != 0), carrying its trailer. */
final class PrismServerException extends PrismException
{
    public int $errorCode;
    public string $sqlstate;
    public string $detail;
    public int $position;

    public function __construct(ErrorInfo $info)
    {
        parent::__construct($info->message !== '' ? $info->message : \sprintf('server error 0x%04x', $info->code));
        $this->errorCode = $info->code;
        $this->sqlstate = $info->sqlstate;
        $this->detail = $info->detail;
        $this->position = $info->position;
    }
}

/** Stable error codes from docs/specs/wire-protocol.md. */
final class ErrorCode
{
    public const PROTOCOL_VIOLATION = 0x0001;
    public const CONNECTION_CLOSED = 0x0002;
    public const AUTHENTICATION_FAILED = 0x0101;
    public const UNAUTHORIZED = 0x0102;
    public const SERIALIZATION_FAILURE = 0x0201;
    public const DEADLOCK = 0x0202;
    public const TRANSACTION_TIMEOUT = 0x0203;
    public const TRANSACTION_ABORTED = 0x0204;
    public const IO_ERROR = 0x0301;
    public const OUT_OF_DISK_SPACE = 0x0302;
    public const SYNTAX_ERROR = 0x0401;
    public const TYPE_ERROR = 0x0402;
    public const OBJECT_NOT_FOUND = 0x0403;
    public const OBJECT_ALREADY_EXISTS = 0x0404;
    public const UNIQUE_VIOLATION = 0x0501;
    public const CHECK_VIOLATION = 0x0502;
    public const OUT_OF_MEMORY = 0x0601;
    public const TOO_MANY_CONNECTIONS = 0x0602;
    public const QUERY_TOO_COMPLEX = 0x0603;
    public const INTERNAL_ERROR = 0xFF01;
}
