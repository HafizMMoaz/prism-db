"""No-server codec round-trip tests. Run with: python -m unittest discover -s tests

These validate the byte layouts against the wire spec without needing a server,
mirroring the Rust `prism-protocol` unit tests and the Node SDK's codec tests."""

import unittest

from prismdb._codec import Reader, Writer, frame_encode
from prismdb.document import decode_document, encode_document
from prismdb.errors import ProtocolError
from prismdb.messages import MSG, decode_packet, encode_packet, sql_body
from prismdb.query import Q, encode_doc_query
from prismdb.update import U, encode_doc_update
from prismdb.value import (
    TAG,
    ObjectId,
    decode_tagged,
    encode_tagged,
    float64,
    int32,
    timestamp,
)


class CodecTest(unittest.TestCase):
    def test_writer_reader_roundtrip(self):
        w = Writer()
        w.u8(0x7F)
        w.u16(0xBEEF)
        w.u32(0xDEADBEEF)
        w.i32(-5)
        w.u64(0x1122334455667788)
        w.i64(-9000000000)
        w.f64(3.5)
        w.u128((1 << 100) + 7)
        w.str_u16("héllo")
        w.bytes_u32(b"\x00\x01\x02")
        r = Reader(w.out())
        self.assertEqual(r.u8(), 0x7F)
        self.assertEqual(r.u16(), 0xBEEF)
        self.assertEqual(r.u32(), 0xDEADBEEF)
        self.assertEqual(r.i32(), -5)
        self.assertEqual(r.u64(), 0x1122334455667788)
        self.assertEqual(r.i64(), -9000000000)
        self.assertEqual(r.f64(), 3.5)
        self.assertEqual(r.u128(), (1 << 100) + 7)
        self.assertEqual(r.str_u16(), "héllo")
        self.assertEqual(r.bytes_u32(), b"\x00\x01\x02")
        r.expect_end()

    def test_truncation_raises(self):
        r = Reader(b"\x01\x02")
        with self.assertRaises(ProtocolError):
            r.u32()

    def test_frame_encode_length_prefix(self):
        framed = frame_encode(b"abcd")
        self.assertEqual(framed[:4], (4).to_bytes(4, "little"))
        self.assertEqual(framed[4:], b"abcd")


class ValueTest(unittest.TestCase):
    def test_scalar_tagged_roundtrip(self):
        cases = [
            (None, None),
            (True, True),
            (False, False),
            (42, 42),
            (-7, -7),
            (3.25, 3.25),
            ("prism", "prism"),
            (b"\xde\xad", b"\xde\xad"),
            (int32(123), 123),
            (float64(5.0), 5.0),
            (timestamp(1_700_000_000_000_000), 1_700_000_000_000_000),
        ]
        for value, expected in cases:
            w = Writer()
            encode_tagged(w, value)
            got = decode_tagged(Reader(w.out()))
            self.assertEqual(got, expected, f"value {value!r}")

    def test_int_maps_to_int64(self):
        w = Writer()
        encode_tagged(w, 1)
        self.assertEqual(w.out()[0], TAG.INT64)

    def test_objectid_hex_roundtrip(self):
        oid = ObjectId.from_hex("507f1f77bcf86cd799439011")
        self.assertEqual(oid.to_hex(), "507f1f77bcf86cd799439011")
        self.assertEqual(str(oid), "507f1f77bcf86cd799439011")
        self.assertEqual(oid, ObjectId(oid.bytes))


class DocumentTest(unittest.TestCase):
    def test_document_roundtrip(self):
        doc = {"name": "carol", "age": 41, "active": True, "score": 9.5}
        encoded = encode_document(doc)
        # total-length prefix matches actual size
        self.assertEqual(int.from_bytes(encoded[:4], "little"), len(encoded))
        self.assertEqual(decode_document(encoded), doc)

    def test_document_rejects_binary(self):
        with self.assertRaises(ProtocolError):
            encode_document({"blob": b"\x00"})


class QueryUpdateTest(unittest.TestCase):
    def test_query_encodes_without_error(self):
        q = Q.and_(Q.eq("city", "NYC"), Q.gt("age", 30), Q.in_("tag", ["a", "b"]))
        blob = encode_doc_query(q)
        self.assertEqual(blob[0], 10)  # AND discriminant

    def test_update_encodes_count_prefix(self):
        ops = [U.set("city", "Boston"), U.inc("age", 1), U.unset("temp")]
        blob = encode_doc_update(ops)
        self.assertEqual(int.from_bytes(blob[:4], "little"), 3)


class MessageTest(unittest.TestCase):
    def test_sql_packet_header(self):
        type_code, body = sql_body("SELECT 1", [], 1)
        packet = encode_packet(7, type_code, body)
        self.assertEqual(packet[0], MSG.SQL_EXECUTE)
        self.assertEqual(int.from_bytes(packet[4:8], "little"), 7)  # request_id

    def test_decode_authack(self):
        w = Writer()
        w.u8(MSG.AUTH_ACK)
        w.raw(b"\x00\x00\x00")
        w.u32(99)  # request id echoed
        w.raw(b"\x00\x00\x00\x00")
        w.u8(0)  # status OK
        w.u64(1234)  # user_oid
        packet = decode_packet(w.out())
        self.assertEqual(packet.request_id, 99)
        self.assertEqual(packet.message.user_oid, 1234)

    def test_decode_error_trailer(self):
        w = Writer()
        w.u8(MSG.TXN_ACK)
        w.raw(b"\x00\x00\x00")
        w.u32(1)
        w.raw(b"\x00\x00\x00\x00")
        w.u8(2)  # status != 0
        w.u64(0)  # txn_id
        w.u64(0)  # commit_lsn
        w.u32(0x0201)
        w.str_u16("serialization failure")
        w.raw(b"40001")
        w.str_u16("")
        w.u32(0)
        packet = decode_packet(w.out())
        self.assertIsNotNone(packet.message.error)
        self.assertEqual(packet.message.error.code, 0x0201)
        self.assertEqual(packet.message.error.sqlstate, "40001")


if __name__ == "__main__":
    unittest.main()
