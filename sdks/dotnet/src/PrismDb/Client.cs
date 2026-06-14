// The high-level client: connect + handshake, then SQL / KV / document calls
// and transaction control. One client owns one connection = one server session,
// so a Begin() ... Commit() brackets the calls in between.

using System;
using System.Collections.Generic;
using System.Text;

namespace PrismDb
{
    /// <summary>A SQL result set. Rows are keyed by column name; Raw keeps cell order.</summary>
    public sealed class SqlResult
    {
        public IReadOnlyList<ColumnDesc> Columns { get; }
        public IReadOnlyList<Dictionary<string, object?>> Rows { get; }
        public IReadOnlyList<object?[]> Raw { get; }
        public ulong AffectedRows { get; }

        internal SqlResult(IReadOnlyList<ColumnDesc> columns, IReadOnlyList<Dictionary<string, object?>> rows,
            IReadOnlyList<object?[]> raw, ulong affectedRows)
        {
            Columns = columns;
            Rows = rows;
            Raw = raw;
            AffectedRows = affectedRows;
        }
    }

    /// <summary>Options for <see cref="Client.Connect(ConnectOptions)"/>.</summary>
    public sealed class ConnectOptions : ConnectionOptions
    {
        public string? Username { get; set; }
        public string? Password { get; set; }
        /// <summary>Bind the session to this database (via FEATURE_CONNECT_DB, or a USE fallback).</summary>
        public string? Database { get; set; }
        public string ClientName { get; set; } = "prismdb-dotnet";
        public string ClientVersion { get; set; } = "0.1.0";
    }

    /// <summary>A connected, authenticated Prism session.</summary>
    public sealed class Client : IDisposable
    {
        private const int ProtocolVersion = 1;
        private static readonly byte[] Empty = Array.Empty<byte>();

        private readonly Connection _conn;

        public KvSurface Kv { get; }
        public DocSurface Doc { get; }

        private Client(Connection conn)
        {
            _conn = conn;
            Kv = new KvSurface(this);
            Doc = new DocSurface(this);
        }

        /// <summary>Connect, perform the handshake, and (if Username is set) authenticate.</summary>
        public static Client Connect(ConnectOptions opts)
        {
            var conn = Connection.Connect(opts);
            var client = new Client(conn);
            try
            {
                bool connectDbHonored = client.Handshake(opts);
                if (!string.IsNullOrEmpty(opts.Database) && !connectDbHonored)
                    client.Sql($"USE {opts.Database}", returnRows: false);
            }
            catch
            {
                conn.Close();
                throw;
            }
            return client;
        }

        /// <summary>Convenience overload: connect with host/port/credentials.</summary>
        public static Client Connect(string host = "127.0.0.1", int port = 4444,
            string? username = null, string? password = null, string? database = null, bool tls = false)
            => Connect(new ConnectOptions
            {
                Host = host,
                Port = port,
                Username = username,
                Password = password,
                Database = database,
                Tls = tls,
            });

        private bool Handshake(ConnectOptions opts)
        {
            string database = opts.Database ?? "";
            long features = database.Length > 0 ? Msg.FeatureConnectDb : 0;
            var (ht, hb) = Protocol.HelloBody(ProtocolVersion, opts.ClientName, opts.ClientVersion, features, database);
            if (_conn.Request(ht, hb) is not HelloAckMsg hello) throw new ProtocolException("expected HelloAck");
            if (hello.Status != 0) Fail(hello.Error);
            bool connectDbHonored = (hello.Features & Msg.FeatureConnectDb) != 0 && database.Length > 0;

            if (opts.Username != null)
            {
                var (at, ab) = Protocol.AuthBody(Msg.AuthPassword, opts.Username, opts.Password ?? "");
                if (_conn.Request(at, ab) is not AuthAckMsg auth) throw new ProtocolException("expected AuthAck");
                if (auth.Status != 0) Fail(auth.Error);
            }
            return connectDbHonored;
        }

        // ---- SQL --------------------------------------------------------------

        /// <summary>Execute a SQL statement. Returns rows for SELECT, counts otherwise.</summary>
        public SqlResult Sql(string text, IReadOnlyList<object?>? parameters = null, bool returnRows = true)
        {
            var (t, b) = Protocol.SqlBody(text, parameters ?? Array.Empty<object?>(), returnRows ? 1 : 0);
            if (_conn.Request(t, b) is not SqlResultMsg reply) throw new ProtocolException("expected SqlResult");
            if (reply.Status != 0) Fail(reply.Error);
            if (reply.MoreFrames) throw new ProtocolException("streamed SQL results are not yet supported");

            var names = new string[reply.Columns.Count];
            for (int i = 0; i < names.Length; i++) names[i] = reply.Columns[i].Name;
            var rows = new List<Dictionary<string, object?>>(reply.Rows.Count);
            foreach (var cells in reply.Rows)
            {
                var obj = new Dictionary<string, object?>(names.Length);
                for (int i = 0; i < names.Length; i++) obj[names[i]] = cells[i];
                rows.Add(obj);
            }
            return new SqlResult(reply.Columns, rows, reply.Rows, reply.AffectedRows);
        }

        // ---- transactions -----------------------------------------------------

        /// <summary>Begin a transaction; returns the assigned transaction id.</summary>
        public ulong Begin(bool readOnly = false)
        {
            var (t, b) = Protocol.BeginBody(readOnly ? Msg.TxnReadOnly : Msg.TxnReadWrite);
            return Txn(t, b).TxnId;
        }

        /// <summary>Commit the current transaction (optionally idempotent).</summary>
        public void Commit(ulong idempotencyKeyLo = 0, ulong idempotencyKeyHi = 0)
        {
            var (t, b) = Protocol.CommitBody(idempotencyKeyLo, idempotencyKeyHi);
            Txn(t, b);
        }

        /// <summary>Abort the current transaction.</summary>
        public void Abort()
        {
            var (t, b) = Protocol.AbortBody();
            Txn(t, b);
        }

        private TxnAckMsg Txn(int typeCode, byte[] body)
        {
            if (_conn.Request(typeCode, body) is not TxnAckMsg reply) throw new ProtocolException("expected TxnAck");
            if (reply.Status != 0) Fail(reply.Error);
            return reply;
        }

        // ---- misc -------------------------------------------------------------

        /// <summary>Round-trip a keep-alive ping.</summary>
        public void Ping()
        {
            var (t, b) = Protocol.PingBody();
            if (_conn.Request(t, b) is not PongMsg) throw new ProtocolException("expected Pong");
        }

        public void Close() => _conn.Close();

        public void Dispose() => Close();

        // ---- internal reply helpers ------------------------------------------

        internal KvResultMsg KvReply(int typeCode, byte[] body)
        {
            if (_conn.Request(typeCode, body) is not KvResultMsg reply) throw new ProtocolException("expected KvResult");
            if (reply.Status != 0) Fail(reply.Error);
            return reply;
        }

        internal DocResultMsg DocReply(int typeCode, byte[] body)
        {
            if (_conn.Request(typeCode, body) is not DocResultMsg reply) throw new ProtocolException("expected DocResult");
            if (reply.Status != 0) Fail(reply.Error);
            if (reply.MoreFrames) throw new ProtocolException("streamed document results are not yet supported");
            return reply;
        }

        internal static byte[] EmptyBlob => Empty;

        private static void Fail(ErrorInfo? error)
        {
            throw new PrismServerException(error ?? new ErrorInfo { Code = 0, Message = "server error", SqlState = "XX000" });
        }

        internal static byte[] ToBytes(object key) => key switch
        {
            string s => Encoding.UTF8.GetBytes(s),
            byte[] b => b,
            _ => throw new ProtocolException("key/value must be string or byte[]"),
        };
    }

    /// <summary>client.Kv — namespaced key/value operations.</summary>
    public sealed class KvSurface
    {
        private readonly Client _c;
        internal KvSurface(Client c) => _c = c;

        public byte[]? Get(string ns, object key)
        {
            var (t, b) = Protocol.KvGetBody(ns, Client.ToBytes(key));
            var reply = _c.KvReply(t, b);
            if (reply.Op != 1) throw new ProtocolException("expected a KV get result");
            return reply.Value;
        }

        public string? GetString(string ns, object key)
        {
            var v = Get(ns, key);
            return v == null ? null : Encoding.UTF8.GetString(v);
        }

        public void Put(string ns, object key, object value)
        {
            var (t, b) = Protocol.KvPutBody(ns, Client.ToBytes(key), Client.ToBytes(value));
            _c.KvReply(t, b);
        }

        public void Delete(string ns, object key)
        {
            var (t, b) = Protocol.KvDeleteBody(ns, Client.ToBytes(key));
            _c.KvReply(t, b);
        }
    }

    /// <summary>client.Doc — document collection operations.</summary>
    public sealed class DocSurface
    {
        private readonly Client _c;
        internal DocSurface(Client c) => _c = c;

        public ObjectId InsertOne(string collection, IDictionary<string, object?> document)
        {
            var (t, b) = Protocol.DocBody(1, collection, new[] { DocumentCodec.Encode(document) });
            var reply = _c.DocReply(t, b);
            if (reply.InsertedIds.Count == 0) throw new ProtocolException("insert returned no _id");
            return reply.InsertedIds[0];
        }

        public IReadOnlyList<ObjectId> InsertMany(string collection, IEnumerable<IDictionary<string, object?>> documents)
        {
            var blobs = new List<byte[]>();
            foreach (var d in documents) blobs.Add(DocumentCodec.Encode(d));
            var (t, b) = Protocol.DocInsertManyBody(collection, blobs);
            return _c.DocReply(t, b).InsertedIds;
        }

        public IReadOnlyList<Document> Find(string collection, DocQuery? query = null)
        {
            var (t, b) = Protocol.DocBody(3, collection, new[] { QueryCodec.Encode(query ?? Q.All()), Client.EmptyBlob });
            var reply = _c.DocReply(t, b);
            var docs = new List<Document>(reply.Docs.Count);
            foreach (var d in reply.Docs) docs.Add(DocumentCodec.Decode(d));
            return docs;
        }

        public Document? FindOne(string collection, DocQuery? query = null)
        {
            var (t, b) = Protocol.DocBody(4, collection, new[] { QueryCodec.Encode(query ?? Q.All()), Client.EmptyBlob });
            var reply = _c.DocReply(t, b);
            return reply.Docs.Count > 0 ? DocumentCodec.Decode(reply.Docs[0]) : null;
        }

        public ulong Count(string collection, DocQuery? query = null)
        {
            var (t, b) = Protocol.DocBody(9, collection, new[] { QueryCodec.Encode(query ?? Q.All()), Client.EmptyBlob });
            return _c.DocReply(t, b).Affected;
        }

        public ulong UpdateOne(string collection, DocQuery query, IReadOnlyList<DocUpdate> update)
            => Update(5, collection, query, update);

        public ulong UpdateMany(string collection, DocQuery query, IReadOnlyList<DocUpdate> update)
            => Update(6, collection, query, update);

        public ulong DeleteOne(string collection, DocQuery query) => Delete(7, collection, query);

        public ulong DeleteMany(string collection, DocQuery query) => Delete(8, collection, query);

        private ulong Update(int op, string collection, DocQuery query, IReadOnlyList<DocUpdate> update)
        {
            var (t, b) = Protocol.DocBody(op, collection,
                new[] { QueryCodec.Encode(query), UpdateCodec.Encode(update), Client.EmptyBlob });
            return _c.DocReply(t, b).Affected;
        }

        private ulong Delete(int op, string collection, DocQuery query)
        {
            var (t, b) = Protocol.DocBody(op, collection, new[] { QueryCodec.Encode(query), Client.EmptyBlob });
            return _c.DocReply(t, b).Affected;
        }
    }
}
