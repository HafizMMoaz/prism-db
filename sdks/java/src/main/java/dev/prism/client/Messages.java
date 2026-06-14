package dev.prism.client;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;

/**
 * Protocol messages: the 12-byte header plus per-message bodies.
 *
 * Mirrors {@code crates/prism-protocol/src/message.rs}. Client request bodies
 * are built inline in {@link Client} with a {@link Writer}; this file owns the
 * shared header encode and the server-message decode.
 */
final class Messages {
    private Messages() {}
}

/** Message-type discriminants (first header byte). */
final class Msg {
    private Msg() {}

    static final int HELLO = 0x01, HELLO_ACK = 0x02, AUTH = 0x03, AUTH_ACK = 0x04;
    static final int BEGIN = 0x10, COMMIT = 0x11, ABORT = 0x12, TXN_ACK = 0x13;
    static final int SQL_EXECUTE = 0x20, SQL_RESULT = 0x21;
    static final int DOC_OP = 0x30, DOC_RESULT = 0x31;
    static final int KV_OP = 0x40, KV_RESULT = 0x41;
    static final int NOTICE = 0x60, PING = 0x70, PONG = 0x71;

    static final int AUTH_PASSWORD = 1;
    static final int TXN_READ_WRITE = 0;
    static final int TXN_READ_ONLY = 1;
    static final int FEATURE_CONNECT_DB = 1;
}

// ---- decoded server messages ------------------------------------------------

final class HelloAckMsg {
    final int status;
    final String serverVersion;
    final long features;
    final ErrorInfo error;
    HelloAckMsg(int status, String serverVersion, long features, ErrorInfo error) {
        this.status = status; this.serverVersion = serverVersion; this.features = features; this.error = error;
    }
}

final class AuthAckMsg {
    final int status;
    final long userOid;
    final ErrorInfo error;
    AuthAckMsg(int status, long userOid, ErrorInfo error) {
        this.status = status; this.userOid = userOid; this.error = error;
    }
}

final class TxnAckMsg {
    final int status;
    final long txnId;
    final long commitLsn;
    final ErrorInfo error;
    TxnAckMsg(int status, long txnId, long commitLsn, ErrorInfo error) {
        this.status = status; this.txnId = txnId; this.commitLsn = commitLsn; this.error = error;
    }
}

final class SqlResultMsg {
    final int status;
    final long affectedRows;
    final List<ColumnDesc> columns;
    final List<Object[]> rows;
    final boolean moreFrames;
    final ErrorInfo error;
    SqlResultMsg(int status, long affectedRows, List<ColumnDesc> columns, List<Object[]> rows, boolean moreFrames, ErrorInfo error) {
        this.status = status; this.affectedRows = affectedRows; this.columns = columns;
        this.rows = rows; this.moreFrames = moreFrames; this.error = error;
    }
}

final class DocResultMsg {
    final int status;
    final long affected;
    final List<ObjectId> insertedIds;
    final List<byte[]> docs;
    final boolean moreFrames;
    final ErrorInfo error;
    DocResultMsg(int status, long affected, List<ObjectId> insertedIds, List<byte[]> docs, boolean moreFrames, ErrorInfo error) {
        this.status = status; this.affected = affected; this.insertedIds = insertedIds;
        this.docs = docs; this.moreFrames = moreFrames; this.error = error;
    }
}

final class KvResultMsg {
    final int status;
    final int op;
    final byte[] value;
    final List<byte[][]> entries;
    final boolean moreFrames;
    final ErrorInfo error;
    KvResultMsg(int status, int op, byte[] value, List<byte[][]> entries, boolean moreFrames, ErrorInfo error) {
        this.status = status; this.op = op; this.value = value;
        this.entries = entries; this.moreFrames = moreFrames; this.error = error;
    }
}

final class NoticeMsg {
    final int severity;
    final int code;
    final String message;
    NoticeMsg(int severity, int code, String message) {
        this.severity = severity; this.code = code; this.message = message;
    }
}

final class PongMsg {}

final class ServerPacket {
    final long requestId;
    final Object message;
    ServerPacket(long requestId, Object message) {
        this.requestId = requestId;
        this.message = message;
    }
}

final class Protocol {
    private Protocol() {}

    static byte[] encodePacket(long requestId, int type, byte[] body) {
        Writer w = new Writer();
        w.u8(type);
        w.u8(0); w.u8(0); w.u8(0); // reserved
        w.u32(requestId);
        w.u32(0);                   // reserved
        w.raw(body);
        return w.out();
    }

    static ServerPacket decodePacket(byte[] payload) {
        Reader r = new Reader(payload);
        int type = r.u8();
        r.skip(3);
        long requestId = r.u32();
        r.skip(4);
        Object message = decodeBody(type, r);
        r.expectEnd();
        return new ServerPacket(requestId, message);
    }

    private static ErrorInfo decodeTrailer(Reader r, int status) {
        if (status == 0) return null;
        int code = (int) r.u32();
        String message = r.strU16();
        String sqlstate = new String(r.raw(5), StandardCharsets.US_ASCII);
        String detail = r.strU16();
        int position = (int) r.u32();
        return new ErrorInfo(code, message, sqlstate, detail, position);
    }

    private static Object decodeBody(int type, Reader r) {
        switch (type) {
            case Msg.HELLO_ACK: {
                int status = r.u8();
                String ver = r.strU16();
                long features = r.u32();
                r.skip(16); // session id
                return new HelloAckMsg(status, ver, features, decodeTrailer(r, status));
            }
            case Msg.AUTH_ACK: {
                int status = r.u8();
                long oid = r.u64();
                return new AuthAckMsg(status, oid, decodeTrailer(r, status));
            }
            case Msg.TXN_ACK: {
                int status = r.u8();
                long txnId = r.u64();
                long lsn = r.u64();
                return new TxnAckMsg(status, txnId, lsn, decodeTrailer(r, status));
            }
            case Msg.SQL_RESULT: {
                int status = r.u8();
                long affected = r.u64();
                int colCount = r.u16();
                List<ColumnDesc> cols = new ArrayList<>(colCount);
                for (int i = 0; i < colCount; i++) {
                    cols.add(new ColumnDesc(r.strU16(), r.u8(), r.u8() != 0));
                }
                long rowCount = r.u32();
                List<Object[]> rows = decodeRows(cols, rowCount, r);
                boolean more = r.u8() != 0;
                return new SqlResultMsg(status, affected, cols, rows, more, decodeTrailer(r, status));
            }
            case Msg.DOC_RESULT: {
                int status = r.u8();
                long affected = r.u64();
                long idCount = r.u32();
                List<ObjectId> ids = new ArrayList<>();
                for (long i = 0; i < idCount; i++) ids.add(new ObjectId(r.raw(12)));
                long docCount = r.u32();
                List<byte[]> docs = new ArrayList<>();
                for (long i = 0; i < docCount; i++) docs.add(r.bytesU32());
                boolean more = r.u8() != 0;
                return new DocResultMsg(status, affected, ids, docs, more, decodeTrailer(r, status));
            }
            case Msg.KV_RESULT: {
                int status = r.u8();
                int op = r.u8();
                return decodeKvBody(status, op, r);
            }
            case Msg.NOTICE:
                return new NoticeMsg(r.u8(), (int) r.u32(), r.strU16());
            case Msg.PONG:
                return new PongMsg();
            default:
                throw new ProtocolException(String.format("unexpected server message type 0x%x", type));
        }
    }

    private static List<Object[]> decodeRows(List<ColumnDesc> columns, long rowCount, Reader r) {
        int nb = (columns.size() + 7) / 8;
        List<Object[]> rows = new ArrayList<>();
        for (long i = 0; i < rowCount; i++) {
            byte[] bitmap = r.raw(nb);
            Object[] row = new Object[columns.size()];
            for (int c = 0; c < columns.size(); c++) {
                boolean isNull = (bitmap[c >> 3] & (1 << (c & 7))) != 0;
                row[c] = isNull ? null : ValueCodec.decodeUntagged(r, columns.get(c).typeTag);
            }
            rows.add(row);
        }
        return rows;
    }

    private static KvResultMsg decodeKvBody(int status, int op, Reader r) {
        switch (op) {
            case 1: {
                boolean found = r.u8() != 0;
                byte[] value = found ? r.bytesU32() : null;
                return new KvResultMsg(status, op, value, null, false, decodeTrailer(r, status));
            }
            case 2:
            case 3:
                return new KvResultMsg(status, op, null, null, false, decodeTrailer(r, status));
            case 4:
            case 5: {
                long count = r.u32();
                List<byte[][]> entries = new ArrayList<>();
                for (long i = 0; i < count; i++) entries.add(new byte[][]{r.bytesU16(), r.bytesU32()});
                boolean more = r.u8() != 0;
                return new KvResultMsg(status, op, null, entries, more, decodeTrailer(r, status));
            }
            default:
                throw new ProtocolException(String.format("unknown kv result op 0x%x", op));
        }
    }
}
