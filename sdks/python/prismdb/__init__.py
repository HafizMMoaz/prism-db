"""prismdb - a pure-Python client for PrismDB over the binary wire protocol
(``docs/specs/wire-protocol.md``). No native build, no C extensions.

    from prismdb import Client, Q, U

    with Client.connect(host="127.0.0.1", port=4444, username="admin", password="admin") as db:
        db.sql("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)")
        db.sql("INSERT INTO users VALUES (1, 'alice')")
        print(db.sql("SELECT * FROM users").rows)
"""

from .client import Client, SqlResult
from .document import Document
from .errors import ErrorCode, ErrorInfo, PrismError, PrismServerError, ProtocolError
from .query import DocQuery, Q
from .update import DocUpdate, U
from .value import TAG, ObjectId, Typed, Value, float64, int32, int64, timestamp

__version__ = "0.1.0"

__all__ = [
    "Client",
    "SqlResult",
    "Document",
    "Q",
    "DocQuery",
    "U",
    "DocUpdate",
    "ObjectId",
    "Typed",
    "Value",
    "TAG",
    "int32",
    "int64",
    "float64",
    "timestamp",
    "PrismError",
    "PrismServerError",
    "ProtocolError",
    "ErrorInfo",
    "ErrorCode",
    "__version__",
]
