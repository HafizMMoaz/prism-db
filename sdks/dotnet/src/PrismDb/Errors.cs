// Error types surfaced by the SDK.

using System;

namespace PrismDb
{
    /// <summary>Base class for every error raised by this SDK.</summary>
    public abstract class PrismException : Exception
    {
        protected PrismException(string message) : base(message) { }
    }

    /// <summary>A malformed frame/message, or a byte-level decode failure.</summary>
    public sealed class ProtocolException : PrismException
    {
        public ProtocolException(string message) : base(message) { }
    }

    /// <summary>The structured error trailer a server attaches to a non-OK response.</summary>
    public sealed class ErrorInfo
    {
        /// <summary>A code from the wire spec's error-code ranges.</summary>
        public int Code { get; set; }
        /// <summary>A human-readable message.</summary>
        public string Message { get; set; } = "";
        /// <summary>The 5-character SQLSTATE (e.g. "23505").</summary>
        public string SqlState { get; set; } = "";
        /// <summary>Optional extra detail (may be empty).</summary>
        public string Detail { get; set; } = "";
        /// <summary>Character position in the source SQL, or 0.</summary>
        public long Position { get; set; }
    }

    /// <summary>An error returned by the server (status != 0), carrying its trailer.</summary>
    public sealed class PrismServerException : PrismException
    {
        public int Code { get; }
        public string SqlState { get; }
        public string Detail { get; }
        public long Position { get; }

        public PrismServerException(ErrorInfo info)
            : base(string.IsNullOrEmpty(info.Message) ? $"server error 0x{info.Code:x4}" : info.Message)
        {
            Code = info.Code;
            SqlState = info.SqlState;
            Detail = info.Detail;
            Position = info.Position;
        }
    }

    /// <summary>Stable error codes from docs/specs/wire-protocol.md.</summary>
    public enum ErrorCode
    {
        ProtocolViolation = 0x0001,
        ConnectionClosed = 0x0002,
        AuthenticationFailed = 0x0101,
        Unauthorized = 0x0102,
        SerializationFailure = 0x0201,
        Deadlock = 0x0202,
        TransactionTimeout = 0x0203,
        TransactionAborted = 0x0204,
        IoError = 0x0301,
        OutOfDiskSpace = 0x0302,
        SyntaxError = 0x0401,
        TypeError = 0x0402,
        ObjectNotFound = 0x0403,
        ObjectAlreadyExists = 0x0404,
        UniqueViolation = 0x0501,
        CheckViolation = 0x0502,
        OutOfMemory = 0x0601,
        TooManyConnections = 0x0602,
        QueryTooComplex = 0x0603,
        InternalError = 0xFF01,
    }
}
