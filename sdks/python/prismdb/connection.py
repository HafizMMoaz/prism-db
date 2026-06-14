"""The transport: a TCP (optionally TLS) socket that frames outgoing messages,
reads back full frames, and matches each reply to its request by the echoed
``request_id``. Server-initiated notices (request_id 0) go to a handler.

The connection is synchronous: each :meth:`request` writes a frame and blocks
until the matching reply arrives, dispatching any notices seen in between."""

from __future__ import annotations

import socket
import ssl
from typing import Callable, Optional, Tuple, Union

from ._codec import frame_encode
from .errors import ProtocolError
from .messages import Notice, decode_packet, encode_packet

NoticeHandler = Callable[[Notice], None]
# ``tls``: False/None = plaintext; True = default client context; an
# ``ssl.SSLContext`` = use it as-is.
TlsArg = Union[bool, ssl.SSLContext, None]


class Connection:
    """A framed, request/reply socket connection to a Prism server."""

    def __init__(self, sock: socket.socket, on_notice: Optional[NoticeHandler] = None) -> None:
        self._sock = sock
        self._on_notice = on_notice
        self._next_id = 1
        self._inbound = bytearray()
        self._closed: Optional[Exception] = None

    @classmethod
    def connect(
        cls,
        host: str = "127.0.0.1",
        port: int = 4444,
        *,
        tls: TlsArg = None,
        server_hostname: Optional[str] = None,
        connect_timeout: float = 10.0,
        on_notice: Optional[NoticeHandler] = None,
    ) -> "Connection":
        """Open a connection (TCP, or TLS when ``tls`` is set)."""
        raw = socket.create_connection((host, port), timeout=connect_timeout)
        raw.settimeout(None)
        raw.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        sock: socket.socket = raw
        if tls:
            ctx = tls if isinstance(tls, ssl.SSLContext) else ssl.create_default_context()
            sock = ctx.wrap_socket(raw, server_hostname=server_hostname or host)
        return cls(sock, on_notice)

    def request(self, type_code: int, body: bytes) -> object:
        """Send a client message and return the matching reply message."""
        if self._closed is not None:
            raise self._closed
        request_id = self._next_id
        self._next_id = 1 if self._next_id >= 0xFFFFFFFF else self._next_id + 1
        try:
            self._sock.sendall(frame_encode(encode_packet(request_id, type_code, body)))
        except OSError as e:
            self._fail(ProtocolError(f"send failed: {e}"))
            raise self._closed  # type: ignore[misc]

        while True:
            payload = self._read_frame()
            packet = decode_packet(payload)
            if isinstance(packet.message, Notice):
                if self._on_notice is not None:
                    self._on_notice(packet.message)
                continue
            if packet.request_id == request_id:
                return packet.message
            # An unmatched reply (e.g. a late response) is ignored.

    def close(self) -> None:
        """Close the connection. Further use raises."""
        self._fail(ProtocolError("connection closed by client"))
        try:
            self._sock.close()
        except OSError:
            pass

    # -- internals ----------------------------------------------------------

    def _read_frame(self) -> bytes:
        header = self._read_exact(4)
        length = int.from_bytes(header, "little")
        return self._read_exact(length)

    def _read_exact(self, n: int) -> bytes:
        # Drain anything already buffered from a previous read first.
        while len(self._inbound) < n:
            try:
                chunk = self._sock.recv(65536)
            except OSError as e:
                self._fail(ProtocolError(f"connection closed by server: {e}"))
                raise self._closed  # type: ignore[misc]
            if not chunk:
                self._fail(ProtocolError("connection closed by server"))
                raise self._closed  # type: ignore[misc]
            self._inbound += chunk
        out = bytes(self._inbound[:n])
        del self._inbound[:n]
        return out

    def _fail(self, err: Exception) -> None:
        if self._closed is None:
            self._closed = err


def split_tls_args(tls: TlsArg) -> Tuple[TlsArg, Optional[str]]:  # pragma: no cover
    """Reserved hook for richer TLS option parsing; identity for now."""
    return tls, None
