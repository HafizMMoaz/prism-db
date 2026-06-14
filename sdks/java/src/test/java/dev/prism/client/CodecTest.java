package dev.prism.client;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.List;
import org.junit.jupiter.api.Test;

/**
 * No-server codec round-trip tests. Run with {@code mvn test}. These validate
 * the byte layouts against the wire spec without needing a server, mirroring the
 * Rust {@code prism-protocol} unit tests and the reference Node SDK's tests.
 */
class CodecTest {

    @Test
    void writerReaderRoundtrip() {
        Writer w = new Writer();
        w.u8(0x7F);
        w.u16(0xBEEF);
        w.u32(0xDEADBEEFL);
        w.i32(-5);
        w.u64(0x1122334455667788L);
        w.i64(-9000000000L);
        w.f64(3.5);
        w.strU16("héllo");
        w.bytesU32(new byte[] {0, 1, 2});

        Reader r = new Reader(w.out());
        assertEquals(0x7F, r.u8());
        assertEquals(0xBEEF, r.u16());
        assertEquals(0xDEADBEEFL, r.u32());
        assertEquals(-5, r.i32());
        assertEquals(0x1122334455667788L, r.u64());
        assertEquals(-9000000000L, r.i64());
        assertEquals(3.5, r.f64(), 1e-12);
        assertEquals("héllo", r.strU16());
        assertArrayEquals(new byte[] {0, 1, 2}, r.bytesU32());
        r.expectEnd();
    }

    @Test
    void truncationThrows() {
        Reader r = new Reader(new byte[] {1, 2});
        assertThrows(ProtocolException.class, r::u32);
    }

    @Test
    void frameLengthPrefix() {
        byte[] framed = Frame.encode("abcd".getBytes(StandardCharsets.US_ASCII));
        assertEquals(8, framed.length);
        assertEquals(4, framed[0] & 0xFF);
        assertEquals(0, framed[1] & 0xFF);
    }

    @Test
    void scalarTaggedRoundtrip() {
        Object[][] cases = {
            {null, null},
            {Boolean.TRUE, Boolean.TRUE},
            {Boolean.FALSE, Boolean.FALSE},
            {42L, 42L},
            {-7L, -7L},
            {3.25, 3.25},
            {"prism", "prism"},
            {Values.int32(123), 123},
            {Values.float64(5.0), 5.0},
            {Values.timestamp(1_700_000_000_000_000L), 1_700_000_000_000_000L},
        };
        for (Object[] c : cases) {
            Writer w = new Writer();
            ValueCodec.encodeTagged(w, c[0]);
            Object got = ValueCodec.decodeTagged(new Reader(w.out()));
            assertEquals(c[1], got);
        }
    }

    @Test
    void intMapsToInt64() {
        Writer w = new Writer();
        ValueCodec.encodeTagged(w, 1);
        assertEquals(Tag.INT64, w.out()[0] & 0xFF);
    }

    @Test
    void binaryRoundtrip() {
        Writer w = new Writer();
        byte[] payload = {(byte) 0xDE, (byte) 0xAD};
        ValueCodec.encodeTagged(w, payload);
        Object got = ValueCodec.decodeTagged(new Reader(w.out()));
        assertArrayEquals(payload, (byte[]) got);
    }

    @Test
    void objectIdHexRoundtrip() {
        ObjectId oid = ObjectId.fromHex("507f1f77bcf86cd799439011");
        assertEquals("507f1f77bcf86cd799439011", oid.toHex());
        assertEquals(oid, new ObjectId(oid.bytes()));
    }

    @Test
    void documentRoundtrip() {
        Document d = new Document().set("name", "carol").set("age", 41L).set("active", true).set("score", 9.5);
        byte[] enc = DocumentCodec.encode(d);
        long total = (enc[0] & 0xFFL) | ((enc[1] & 0xFFL) << 8) | ((enc[2] & 0xFFL) << 16) | ((enc[3] & 0xFFL) << 24);
        assertEquals(enc.length, total);
        Document back = DocumentCodec.decode(enc);
        assertEquals("carol", back.get("name"));
        assertEquals(41L, back.get("age"));
    }

    @Test
    void documentRejectsBinary() {
        Document d = new Document().set("blob", new byte[] {0});
        assertThrows(ProtocolException.class, () -> DocumentCodec.encode(d));
    }

    @Test
    void queryAndDiscriminant() {
        DocQuery q = Q.and(Q.eq("city", "NYC"), Q.gt("age", 30L), Q.in("tag", Arrays.asList("a", "b")));
        assertEquals(10, QueryCodec.encode(q)[0] & 0xFF);
    }

    @Test
    void updateCountPrefix() {
        List<DocUpdate> ops = Arrays.asList(U.set("city", "Boston"), U.inc("age", 1), U.unset("temp"));
        byte[] blob = UpdateCodec.encode(ops);
        long count = (blob[0] & 0xFFL) | ((blob[1] & 0xFFL) << 8) | ((blob[2] & 0xFFL) << 16) | ((blob[3] & 0xFFL) << 24);
        assertEquals(3, count);
    }

    @Test
    void sqlPacketHeader() {
        Writer body = new Writer();
        body.strU32("SELECT 1");
        body.u16(0);
        body.u32(1);
        byte[] packet = Protocol.encodePacket(7, Msg.SQL_EXECUTE, body.out());
        assertEquals(Msg.SQL_EXECUTE, packet[0] & 0xFF);
        long reqId = (packet[4] & 0xFFL) | ((packet[5] & 0xFFL) << 8) | ((packet[6] & 0xFFL) << 16) | ((packet[7] & 0xFFL) << 24);
        assertEquals(7, reqId);
    }

    @Test
    void decodeAuthAck() {
        Writer w = new Writer();
        w.u8(Msg.AUTH_ACK);
        w.u8(0); w.u8(0); w.u8(0);
        w.u32(99);
        w.u32(0);
        w.u8(0);          // status OK
        w.u64(1234);      // user_oid
        ServerPacket p = Protocol.decodePacket(w.out());
        assertEquals(99, p.requestId);
        assertTrue(p.message instanceof AuthAckMsg);
        assertEquals(1234, ((AuthAckMsg) p.message).userOid);
    }

    @Test
    void decodeErrorTrailer() {
        Writer w = new Writer();
        w.u8(Msg.TXN_ACK);
        w.u8(0); w.u8(0); w.u8(0);
        w.u32(1);
        w.u32(0);
        w.u8(2);          // status != 0
        w.u64(0);         // txn id
        w.u64(0);         // commit lsn
        w.u32(0x0201);
        w.strU16("serialization failure");
        w.raw("40001".getBytes(StandardCharsets.US_ASCII));
        w.strU16("");
        w.u32(0);
        ServerPacket p = Protocol.decodePacket(w.out());
        TxnAckMsg m = (TxnAckMsg) p.message;
        assertNotNull(m.error);
        assertEquals(0x0201, m.error.code);
        assertEquals("40001", m.error.sqlstate);
    }
}
