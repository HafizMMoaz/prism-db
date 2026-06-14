// Protocol messages: the 12-byte header plus per-message bodies.
//
// Mirrors crates/prism-protocol/src/message.rs. The SDK encodes client messages
// (as (typeCode, body) pairs) and decodes server messages; both directions share
// the header and the error trailer.

using System;
using System.Collections.Generic;

namespace PrismDb
{
    internal static class Msg
    {
        public const int Hello = 0x01, HelloAck = 0x02, Auth = 0x03, AuthAck = 0x04;
        public const int Begin = 0x10, Commit = 0x11, Abort = 0x12, TxnAck = 0x13;
        public const int SqlExecute = 0x20, SqlResult = 0x21;
        public const int DocOp = 0x30, DocResult = 0x31;
        public const int KvOp = 0x40, KvResult = 0x41;
        public const int Cancel = 0x50, Notice = 0x60, Ping = 0x70, Pong = 0x71;

        public const int AuthPassword = 1;
        public const int TxnReadWrite = 0;
        public const int TxnReadOnly = 1;
        public const int FeatureConnectDb = 1 << 0;
    }

    public sealed class ColumnDesc
    {
        public string Name { get; set; } = "";
        public int TypeTag { get; set; }
        public bool Nullable { get; set; }
    }

    // ---- decoded server messages --------------------------------------------

    internal abstract class ServerMessage
    {
        public int Status;
        public ErrorInfo? Error;
    }

    internal sealed class HelloAckMsg : ServerMessage
    {
        public string ServerVersion = "";
        public long Features;
    }

    internal sealed class AuthAckMsg : ServerMessage { public ulong UserOid; }

    internal sealed class TxnAckMsg : ServerMessage { public ulong TxnId; public ulong CommitLsn; }

    internal sealed class SqlResultMsg : ServerMessage
    {
        public ulong AffectedRows;
        public List<ColumnDesc> Columns = new();
        public List<object?[]> Rows = new();
        public bool MoreFrames;
    }

    internal sealed class DocResultMsg : ServerMessage
    {
        public ulong Affected;
        public List<ObjectId> InsertedIds = new();
        public List<byte[]> Docs = new();
        public bool MoreFrames;
    }

    internal sealed class KvResultMsg : ServerMessage
    {
        public int Op;
        public byte[]? Value;
        public List<(byte[] Key, byte[] Val)> Entries = new();
        public bool MoreFrames;
    }

    internal sealed class NoticeMsg : ServerMessage { public int Severity; public int Code; public string Message = ""; }

    internal sealed class PongMsg : ServerMessage { }

    internal sealed class ServerPacket
    {
        public long RequestId;
        public ServerMessage Message = null!;
    }

    internal static class Protocol
    {
        private static readonly byte[] Reserved3 = new byte[3];
        private static readonly byte[] Reserved4 = new byte[4];

        public static byte[] EncodePacket(long requestId, int typeCode, byte[] body)
        {
            var w = new Writer();
            w.U8(typeCode);
            w.Raw(Reserved3);
            w.U32(requestId);
            w.Raw(Reserved4);
            w.Raw(body);
            return w.Out();
        }

        // ---- client bodies ---------------------------------------------------

        public static (int, byte[]) HelloBody(int protocolVersion, string clientName, string clientVersion,
            long features, string database)
        {
            var w = new Writer();
            w.U32(protocolVersion);
            w.StrU16(clientName);
            w.StrU16(clientVersion);
            w.U32(features);
            if ((features & Msg.FeatureConnectDb) != 0) w.StrU16(database);
            return (Msg.Hello, w.Out());
        }

        public static (int, byte[]) AuthBody(int mechanism, string username, string password)
        {
            var w = new Writer();
            w.U8(mechanism);
            w.StrU16(username);
            if (mechanism == Msg.AuthPassword) w.StrU16(password);
            return (Msg.Auth, w.Out());
        }

        public static (int, byte[]) BeginBody(int mode)
        {
            var w = new Writer();
            w.U8(mode);
            return (Msg.Begin, w.Out());
        }

        public static (int, byte[]) CommitBody(ulong lo, ulong hi)
        {
            var w = new Writer();
            w.U128(lo, hi);
            return (Msg.Commit, w.Out());
        }

        public static (int, byte[]) AbortBody() => (Msg.Abort, Array.Empty<byte>());

        public static (int, byte[]) SqlBody(string sql, IReadOnlyList<object?> parameters, int options)
        {
            var w = new Writer();
            w.StrU32(sql);
            w.U16(parameters.Count);
            foreach (var p in parameters) ValueCodec.EncodeTagged(w, p);
            w.U32(options);
            return (Msg.SqlExecute, w.Out());
        }

        public static (int, byte[]) DocBody(int op, string collection, IReadOnlyList<byte[]> blobs)
        {
            var w = new Writer();
            w.U8(op);
            w.StrU16(collection);
            foreach (var b in blobs) w.BytesU32(b);
            return (Msg.DocOp, w.Out());
        }

        public static (int, byte[]) DocInsertManyBody(string collection, IReadOnlyList<byte[]> docs)
        {
            var w = new Writer();
            w.U8(2);
            w.StrU16(collection);
            w.U32(docs.Count);
            foreach (var d in docs) w.BytesU32(d);
            return (Msg.DocOp, w.Out());
        }

        public static (int, byte[]) KvGetBody(string ns, byte[] key)
        {
            var w = new Writer();
            w.U8(1);
            w.StrU16(ns);
            w.BytesU16(key);
            return (Msg.KvOp, w.Out());
        }

        public static (int, byte[]) KvPutBody(string ns, byte[] key, byte[] value)
        {
            var w = new Writer();
            w.U8(2);
            w.StrU16(ns);
            w.BytesU16(key);
            w.BytesU32(value);
            return (Msg.KvOp, w.Out());
        }

        public static (int, byte[]) KvDeleteBody(string ns, byte[] key)
        {
            var w = new Writer();
            w.U8(3);
            w.StrU16(ns);
            w.BytesU16(key);
            return (Msg.KvOp, w.Out());
        }

        public static (int, byte[]) PingBody() => (Msg.Ping, Array.Empty<byte>());

        // ---- server decode ---------------------------------------------------

        public static ServerPacket DecodePacket(byte[] payload)
        {
            var r = new Reader(payload);
            int type = r.U8();
            r.Raw(3);
            long requestId = r.U32();
            r.Raw(4);
            var message = DecodeBody(type, r);
            r.ExpectEnd();
            return new ServerPacket { RequestId = requestId, Message = message };
        }

        private static ErrorInfo? DecodeTrailer(Reader r, int status)
        {
            if (status == 0) return null;
            return new ErrorInfo
            {
                Code = (int)r.U32(),
                Message = r.StrU16(),
                SqlState = System.Text.Encoding.ASCII.GetString(r.Raw(5)),
                Detail = r.StrU16(),
                Position = r.U32(),
            };
        }

        private static ServerMessage DecodeBody(int type, Reader r)
        {
            switch (type)
            {
                case Msg.HelloAck:
                {
                    int status = r.U8();
                    string ver = r.StrU16();
                    long features = r.U32();
                    r.U128(); // session id
                    return new HelloAckMsg { Status = status, ServerVersion = ver, Features = features, Error = DecodeTrailer(r, status) };
                }
                case Msg.AuthAck:
                {
                    int status = r.U8();
                    ulong oid = r.U64();
                    return new AuthAckMsg { Status = status, UserOid = oid, Error = DecodeTrailer(r, status) };
                }
                case Msg.TxnAck:
                {
                    int status = r.U8();
                    ulong txnId = r.U64();
                    ulong lsn = r.U64();
                    return new TxnAckMsg { Status = status, TxnId = txnId, CommitLsn = lsn, Error = DecodeTrailer(r, status) };
                }
                case Msg.SqlResult:
                {
                    int status = r.U8();
                    ulong affected = r.U64();
                    int colCount = r.U16();
                    var cols = new List<ColumnDesc>(colCount);
                    for (int i = 0; i < colCount; i++)
                        cols.Add(new ColumnDesc { Name = r.StrU16(), TypeTag = r.U8(), Nullable = r.U8() != 0 });
                    long rowCount = r.U32();
                    var rows = DecodeRows(cols, rowCount, r);
                    bool more = r.U8() != 0;
                    return new SqlResultMsg { Status = status, AffectedRows = affected, Columns = cols, Rows = rows, MoreFrames = more, Error = DecodeTrailer(r, status) };
                }
                case Msg.DocResult:
                {
                    int status = r.U8();
                    ulong affected = r.U64();
                    long idCount = r.U32();
                    var ids = new List<ObjectId>();
                    for (long i = 0; i < idCount; i++) ids.Add(new ObjectId(r.Raw(12)));
                    long docCount = r.U32();
                    var docs = new List<byte[]>();
                    for (long i = 0; i < docCount; i++) docs.Add(r.BytesU32());
                    bool more = r.U8() != 0;
                    return new DocResultMsg { Status = status, Affected = affected, InsertedIds = ids, Docs = docs, MoreFrames = more, Error = DecodeTrailer(r, status) };
                }
                case Msg.KvResult:
                {
                    int status = r.U8();
                    int op = r.U8();
                    return DecodeKvBody(status, op, r);
                }
                case Msg.Notice:
                    return new NoticeMsg { Severity = r.U8(), Code = (int)r.U32(), Message = r.StrU16() };
                case Msg.Pong:
                    return new PongMsg();
                default:
                    throw new ProtocolException($"unexpected server message type 0x{type:x}");
            }
        }

        private static List<object?[]> DecodeRows(List<ColumnDesc> columns, long rowCount, Reader r)
        {
            int nb = (columns.Count + 7) / 8;
            var rows = new List<object?[]>();
            for (long i = 0; i < rowCount; i++)
            {
                var bitmap = r.Raw(nb);
                var row = new object?[columns.Count];
                for (int c = 0; c < columns.Count; c++)
                {
                    bool isNull = (bitmap[c >> 3] & (1 << (c & 7))) != 0;
                    row[c] = isNull ? null : ValueCodec.DecodeUntagged(r, columns[c].TypeTag);
                }
                rows.Add(row);
            }
            return rows;
        }

        private static KvResultMsg DecodeKvBody(int status, int op, Reader r)
        {
            switch (op)
            {
                case 1:
                {
                    bool found = r.U8() != 0;
                    byte[]? value = found ? r.BytesU32() : null;
                    return new KvResultMsg { Status = status, Op = op, Value = value, Error = DecodeTrailer(r, status) };
                }
                case 2:
                case 3:
                    return new KvResultMsg { Status = status, Op = op, Error = DecodeTrailer(r, status) };
                case 4:
                case 5:
                {
                    long count = r.U32();
                    var entries = new List<(byte[], byte[])>();
                    for (long i = 0; i < count; i++) entries.Add((r.BytesU16(), r.BytesU32()));
                    bool more = r.U8() != 0;
                    return new KvResultMsg { Status = status, Op = op, Entries = entries, MoreFrames = more, Error = DecodeTrailer(r, status) };
                }
                default:
                    throw new ProtocolException($"unknown kv result op 0x{op:x}");
            }
        }
    }
}
