package dev.prism.client;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.function.Consumer;

/**
 * The high-level client: connect + handshake, then SQL / KV / document calls and
 * transaction control. One client owns one connection = one server session, so a
 * {@link #begin()} … {@link #commit()} brackets the calls in between.
 *
 * <pre>{@code
 * try (Client db = Client.builder().username("admin").password("admin").connect()) {
 *     db.sql("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)");
 *     db.sql("INSERT INTO users VALUES ($1, $2)", List.of(1L, "alice"));
 *     for (var row : db.sql("SELECT * FROM users").rows) System.out.println(row);
 * }
 * }</pre>
 */
public final class Client implements AutoCloseable {
    private static final int PROTOCOL_VERSION = 1;
    private static final byte[] EMPTY = new byte[0];

    private final Connection conn;

    /** Namespaced key/value operations. */
    public final Kv kv = new Kv();
    /** Document collection operations. */
    public final Doc doc = new Doc();

    private Client(Connection conn) {
        this.conn = conn;
    }

    // ---- connect ----------------------------------------------------------

    public static Builder builder() {
        return new Builder();
    }

    /** Convenience: connect with host/port/credentials and otherwise default options. */
    public static Client connect(String host, int port, String username, String password) {
        return builder().host(host).port(port).username(username).password(password).connect();
    }

    /** Fluent connection options. */
    public static final class Builder {
        private String host = "127.0.0.1";
        private int port = 4444;
        private String username;        // null = skip authentication
        private String password = "";
        private String database = "";
        private boolean tls = false;
        private int connectTimeoutMs = 10000;
        private String clientName = "prismdb-java";
        private String clientVersion = "0.1.0";
        private Consumer<NoticeMsg> onNotice;

        public Builder host(String v) { this.host = v; return this; }
        public Builder port(int v) { this.port = v; return this; }
        public Builder username(String v) { this.username = v; return this; }
        public Builder password(String v) { this.password = v; return this; }
        public Builder database(String v) { this.database = v == null ? "" : v; return this; }
        public Builder tls(boolean v) { this.tls = v; return this; }
        public Builder connectTimeoutMs(int v) { this.connectTimeoutMs = v; return this; }
        public Builder clientName(String v) { this.clientName = v; return this; }
        public Builder clientVersion(String v) { this.clientVersion = v; return this; }

        public Client connect() {
            Connection conn = Connection.open(host, port, tls, connectTimeoutMs, onNotice);
            Client client = new Client(conn);
            try {
                boolean honored = client.handshake(username, password, database, clientName, clientVersion);
                if (!database.isEmpty() && !honored) client.sql("USE " + database);
            } catch (RuntimeException e) {
                conn.close();
                throw e;
            }
            return client;
        }
    }

    private boolean handshake(String username, String password, String database, String clientName, String clientVersion) {
        long features = database.isEmpty() ? 0 : Msg.FEATURE_CONNECT_DB;
        Writer b = new Writer();
        b.u32(PROTOCOL_VERSION);
        b.strU16(clientName);
        b.strU16(clientVersion);
        b.u32(features);
        if ((features & Msg.FEATURE_CONNECT_DB) != 0) b.strU16(database);

        Object hello = conn.request(Msg.HELLO, b.out());
        if (!(hello instanceof HelloAckMsg)) throw new ProtocolException("expected HelloAck");
        HelloAckMsg ha = (HelloAckMsg) hello;
        if (ha.status != 0) fail(ha.error);
        boolean connectDbHonored = (ha.features & Msg.FEATURE_CONNECT_DB) != 0 && !database.isEmpty();

        if (username != null) {
            Writer a = new Writer();
            a.u8(Msg.AUTH_PASSWORD);
            a.strU16(username);
            a.strU16(password == null ? "" : password);
            Object auth = conn.request(Msg.AUTH, a.out());
            if (!(auth instanceof AuthAckMsg)) throw new ProtocolException("expected AuthAck");
            AuthAckMsg aa = (AuthAckMsg) auth;
            if (aa.status != 0) fail(aa.error);
        }
        return connectDbHonored;
    }

    // ---- SQL --------------------------------------------------------------

    public SqlResult sql(String text) {
        return sql(text, Collections.emptyList());
    }

    /** Execute a SQL statement with positional parameters ($1, $2, ...). */
    public SqlResult sql(String text, List<?> params) {
        Writer b = new Writer();
        b.strU32(text);
        b.u16(params.size());
        for (Object p : params) ValueCodec.encodeTagged(b, p);
        b.u32(1); // options: return_rows

        Object reply = conn.request(Msg.SQL_EXECUTE, b.out());
        if (!(reply instanceof SqlResultMsg)) throw new ProtocolException("expected SqlResult");
        SqlResultMsg m = (SqlResultMsg) reply;
        if (m.status != 0) fail(m.error);
        if (m.moreFrames) throw new ProtocolException("streamed SQL results are not yet supported");

        List<Map<String, Object>> rows = new ArrayList<>(m.rows.size());
        for (Object[] cells : m.rows) {
            Map<String, Object> obj = new LinkedHashMap<>();
            for (int i = 0; i < m.columns.size(); i++) obj.put(m.columns.get(i).name, cells[i]);
            rows.add(obj);
        }
        return new SqlResult(m.columns, rows, m.rows, m.affectedRows);
    }

    // ---- transactions -----------------------------------------------------

    public long begin() {
        return begin(false);
    }

    /** Begin a transaction; returns the assigned transaction id. */
    public long begin(boolean readOnly) {
        Writer b = new Writer();
        b.u8(readOnly ? Msg.TXN_READ_ONLY : Msg.TXN_READ_WRITE);
        return txn(Msg.BEGIN, b.out()).txnId;
    }

    public void commit() {
        commit(0L);
    }

    /** Commit the current transaction (optionally idempotent). */
    public void commit(long idempotencyKey) {
        Writer b = new Writer();
        b.u128(idempotencyKey);
        txn(Msg.COMMIT, b.out());
    }

    public void abort() {
        txn(Msg.ABORT, EMPTY);
    }

    private TxnAckMsg txn(int type, byte[] body) {
        Object reply = conn.request(type, body);
        if (!(reply instanceof TxnAckMsg)) throw new ProtocolException("expected TxnAck");
        TxnAckMsg m = (TxnAckMsg) reply;
        if (m.status != 0) fail(m.error);
        return m;
    }

    // ---- misc -------------------------------------------------------------

    /** Round-trip a keep-alive ping. */
    public void ping() {
        Object reply = conn.request(Msg.PING, EMPTY);
        if (!(reply instanceof PongMsg)) throw new ProtocolException("expected Pong");
    }

    @Override
    public void close() {
        conn.close();
    }

    // ---- internal reply helpers ------------------------------------------

    private KvResultMsg kvReply(byte[] body) {
        Object reply = conn.request(Msg.KV_OP, body);
        if (!(reply instanceof KvResultMsg)) throw new ProtocolException("expected KvResult");
        KvResultMsg m = (KvResultMsg) reply;
        if (m.status != 0) fail(m.error);
        return m;
    }

    private DocResultMsg docReply(byte[] body) {
        Object reply = conn.request(Msg.DOC_OP, body);
        if (!(reply instanceof DocResultMsg)) throw new ProtocolException("expected DocResult");
        DocResultMsg m = (DocResultMsg) reply;
        if (m.status != 0) fail(m.error);
        if (m.moreFrames) throw new ProtocolException("streamed document results are not yet supported");
        return m;
    }

    private static void fail(ErrorInfo error) {
        throw new ServerException(error != null ? error : new ErrorInfo(0, "server error", "XX000", "", 0));
    }

    private static byte[] utf8(String s) {
        return s.getBytes(StandardCharsets.UTF_8);
    }

    // ---- KV surface -------------------------------------------------------

    /** {@code client.kv} — namespaced key/value operations. */
    public final class Kv {
        public byte[] get(String namespace, byte[] key) {
            Writer b = new Writer();
            b.u8(1);
            b.strU16(namespace);
            b.bytesU16(key);
            KvResultMsg m = kvReply(b.out());
            if (m.op != 1) throw new ProtocolException("expected a KV get result");
            return m.value;
        }

        public byte[] get(String namespace, String key) {
            return get(namespace, utf8(key));
        }

        public String getString(String namespace, String key) {
            byte[] v = get(namespace, utf8(key));
            return v == null ? null : new String(v, StandardCharsets.UTF_8);
        }

        public void put(String namespace, byte[] key, byte[] value) {
            Writer b = new Writer();
            b.u8(2);
            b.strU16(namespace);
            b.bytesU16(key);
            b.bytesU32(value);
            kvReply(b.out());
        }

        public void put(String namespace, String key, String value) {
            put(namespace, utf8(key), utf8(value));
        }

        public void put(String namespace, String key, byte[] value) {
            put(namespace, utf8(key), value);
        }

        public void delete(String namespace, byte[] key) {
            Writer b = new Writer();
            b.u8(3);
            b.strU16(namespace);
            b.bytesU16(key);
            kvReply(b.out());
        }

        public void delete(String namespace, String key) {
            delete(namespace, utf8(key));
        }
    }

    // ---- Document surface -------------------------------------------------

    /** {@code client.doc} — document collection operations. */
    public final class Doc {
        public ObjectId insertOne(String collection, Map<String, Object> document) {
            Writer b = new Writer();
            b.u8(1);
            b.strU16(collection);
            b.bytesU32(DocumentCodec.encode(document));
            DocResultMsg m = docReply(b.out());
            if (m.insertedIds.isEmpty()) throw new ProtocolException("insert returned no _id");
            return m.insertedIds.get(0);
        }

        public List<ObjectId> insertMany(String collection, List<? extends Map<String, Object>> documents) {
            Writer b = new Writer();
            b.u8(2);
            b.strU16(collection);
            b.u32(documents.size());
            for (Map<String, Object> d : documents) b.bytesU32(DocumentCodec.encode(d));
            return docReply(b.out()).insertedIds;
        }

        public List<Document> find(String collection) {
            return find(collection, Q.all());
        }

        public List<Document> find(String collection, DocQuery query) {
            DocResultMsg m = docReply(findBody(3, collection, query));
            List<Document> out = new ArrayList<>(m.docs.size());
            for (byte[] d : m.docs) out.add(DocumentCodec.decode(d));
            return out;
        }

        public Document findOne(String collection) {
            return findOne(collection, Q.all());
        }

        public Document findOne(String collection, DocQuery query) {
            DocResultMsg m = docReply(findBody(4, collection, query));
            return m.docs.isEmpty() ? null : DocumentCodec.decode(m.docs.get(0));
        }

        public long count(String collection) {
            return count(collection, Q.all());
        }

        public long count(String collection, DocQuery query) {
            return docReply(findBody(9, collection, query)).affected;
        }

        public long updateOne(String collection, DocQuery query, List<DocUpdate> update) {
            return docReply(updateBody(5, collection, query, update)).affected;
        }

        public long updateMany(String collection, DocQuery query, List<DocUpdate> update) {
            return docReply(updateBody(6, collection, query, update)).affected;
        }

        public long deleteOne(String collection, DocQuery query) {
            return docReply(findBody(7, collection, query)).affected;
        }

        public long deleteMany(String collection, DocQuery query) {
            return docReply(findBody(8, collection, query)).affected;
        }

        private byte[] findBody(int op, String collection, DocQuery query) {
            Writer b = new Writer();
            b.u8(op);
            b.strU16(collection);
            b.bytesU32(QueryCodec.encode(query));
            b.bytesU32(EMPTY); // options
            return b.out();
        }

        private byte[] updateBody(int op, String collection, DocQuery query, List<DocUpdate> update) {
            Writer b = new Writer();
            b.u8(op);
            b.strU16(collection);
            b.bytesU32(QueryCodec.encode(query));
            b.bytesU32(UpdateCodec.encode(update));
            b.bytesU32(EMPTY); // options
            return b.out();
        }
    }
}
