"""Error types surfaced by the SDK."""

from __future__ import annotations

from dataclasses import dataclass


class PrismError(Exception):
    """Base class for every error raised by this SDK."""


class ProtocolError(PrismError):
    """A malformed frame/message, or a byte-level decode failure."""


@dataclass
class ErrorInfo:
    """The structured error trailer a server attaches to a non-OK response."""

    code: int
    """A code from the wire spec's error-code ranges."""
    message: str
    """A human-readable message."""
    sqlstate: str
    """The 5-character SQLSTATE (e.g. ``"23505"``)."""
    detail: str = ""
    """Optional extra detail (may be empty)."""
    position: int = 0
    """Character position in the source SQL, or 0."""


class PrismServerError(PrismError):
    """An error returned by the server (status != 0), carrying its trailer."""

    def __init__(self, info: ErrorInfo) -> None:
        super().__init__(info.message or f"server error 0x{info.code:04x}")
        self.code = info.code
        self.sqlstate = info.sqlstate
        self.detail = info.detail
        self.position = info.position

    def __repr__(self) -> str:  # pragma: no cover - debugging aid
        return (
            f"PrismServerError(code=0x{self.code:04x}, sqlstate={self.sqlstate!r}, "
            f"message={str(self)!r})"
        )


# Stable error codes from docs/specs/wire-protocol.md and specs/sdk-api.md.
class ErrorCode:
    # Protocol
    PROTOCOL_VIOLATION = 0x0001
    CONNECTION_CLOSED = 0x0002
    # Auth
    AUTHENTICATION_FAILED = 0x0101
    UNAUTHORIZED = 0x0102
    # Transactions
    SERIALIZATION_FAILURE = 0x0201
    DEADLOCK = 0x0202
    TRANSACTION_TIMEOUT = 0x0203
    TRANSACTION_ABORTED = 0x0204
    # Storage
    IO_ERROR = 0x0301
    OUT_OF_DISK_SPACE = 0x0302
    # Query
    SYNTAX_ERROR = 0x0401
    TYPE_ERROR = 0x0402
    OBJECT_NOT_FOUND = 0x0403
    OBJECT_ALREADY_EXISTS = 0x0404
    # Constraint
    UNIQUE_VIOLATION = 0x0501
    CHECK_VIOLATION = 0x0502
    # Resource
    OUT_OF_MEMORY = 0x0601
    TOO_MANY_CONNECTIONS = 0x0602
    QUERY_TOO_COMPLEX = 0x0603
    # Internal
    INTERNAL_ERROR = 0xFF01
