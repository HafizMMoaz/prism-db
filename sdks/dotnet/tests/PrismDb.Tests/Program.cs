// No-server codec round-trip tests. Run with: dotnet run --project tests/PrismDb.Tests
//
// A tiny zero-dependency harness (no xUnit/NUnit) so it builds and runs offline.
// Exits 0 if all checks pass, 1 otherwise.

using System;
using System.Collections.Generic;
using System.Text;
using PrismDb;

internal static class Tests
{
    private static int _failures;

    private static void Check(bool cond, string name)
    {
        Console.WriteLine((cond ? "ok   " : "FAIL ") + name);
        if (!cond) _failures++;
    }

    private static int Main()
    {
        WriterReaderRoundtrip();
        FrameLengthPrefix();
        ScalarTaggedRoundtrip();
        IntMapsToInt64();
        ObjectIdHexRoundtrip();
        DocumentRoundtrip();
        DocumentRejectsBinary();
        QueryEncodes();
        UpdateCountPrefix();
        SqlPacketHeader();
        DecodeAuthAck();
        DecodeErrorTrailer();

        Console.WriteLine();
        Console.WriteLine(_failures == 0 ? "ALL TESTS PASSED" : $"{_failures} TEST(S) FAILED");
        return _failures == 0 ? 0 : 1;
    }

    private static void WriterReaderRoundtrip()
    {
        var w = new Writer();
        w.U8(0x7F);
        w.U16(0xBEEF);
        w.U32(0xDEADBEEF);
        w.I32(-5);
        w.U64(0x1122334455667788);
        w.I64(-9000000000L);
        w.F64(3.5);
        w.StrU16("héllo");
        w.BytesU32(new byte[] { 0, 1, 2 });
        var r = new Reader(w.Out());
        Check(r.U8() == 0x7F, "u8 roundtrip");
        Check(r.U16() == 0xBEEF, "u16 roundtrip");
        Check(r.U32() == 0xDEADBEEF, "u32 roundtrip");
        Check(r.I32() == -5, "i32 roundtrip");
        Check(r.U64() == 0x1122334455667788, "u64 roundtrip");
        Check(r.I64() == -9000000000L, "i64 roundtrip");
        Check(Math.Abs(r.F64() - 3.5) < 1e-12, "f64 roundtrip");
        Check(r.StrU16() == "héllo", "strU16 roundtrip (utf-8)");
        var b = r.BytesU32();
        Check(b.Length == 3 && b[2] == 2, "bytesU32 roundtrip");
        r.ExpectEnd();
    }

    private static void FrameLengthPrefix()
    {
        var framed = Frame.Encode(Encoding.ASCII.GetBytes("abcd"));
        Check(framed.Length == 8 && framed[0] == 4 && framed[1] == 0, "frame length prefix");
    }

    private static void ScalarTaggedRoundtrip()
    {
        var cases = new (object?, object?)[]
        {
            (null, null),
            (true, true),
            (false, false),
            (42L, 42L),
            (-7L, -7L),
            (3.25, 3.25),
            ("prism", "prism"),
            (new byte[] { 0xDE, 0xAD }, new byte[] { 0xDE, 0xAD }),
            (Prism.Int32(123), 123),
            (Prism.Float64(5.0), 5.0),
            (Prism.Timestamp(1_700_000_000_000_000L), 1_700_000_000_000_000L),
        };
        foreach (var (value, expected) in cases)
        {
            var w = new Writer();
            ValueCodec.EncodeTagged(w, value);
            var got = ValueCodec.DecodeTagged(new Reader(w.Out()));
            bool eq = expected is byte[] eb && got is byte[] gb
                ? System.Linq.Enumerable.SequenceEqual(eb, gb)
                : Equals(Convert.ToString(got), Convert.ToString(expected));
            Check(eq, $"scalar tagged roundtrip: {value ?? "null"}");
        }
    }

    private static void IntMapsToInt64()
    {
        var w = new Writer();
        ValueCodec.EncodeTagged(w, 1);
        Check(w.Out()[0] == Tag.Int64, "int maps to Int64 by default");
    }

    private static void ObjectIdHexRoundtrip()
    {
        var oid = ObjectId.FromHex("507f1f77bcf86cd799439011");
        Check(oid.ToHex() == "507f1f77bcf86cd799439011", "objectid hex roundtrip");
        Check(oid.Equals(new ObjectId(oid.Bytes)), "objectid equality");
    }

    private static void DocumentRoundtrip()
    {
        var doc = new Document { ["name"] = "carol", ["age"] = 41L, ["active"] = true, ["score"] = 9.5 };
        var encoded = DocumentCodec.Encode(doc);
        long total = encoded[0] | ((long)encoded[1] << 8) | ((long)encoded[2] << 16) | ((long)encoded[3] << 24);
        Check(total == encoded.Length, "document total-length prefix");
        var back = DocumentCodec.Decode(encoded);
        Check((string?)back["name"] == "carol" && (long)back["age"]! == 41L, "document roundtrip");
    }

    private static void DocumentRejectsBinary()
    {
        bool threw = false;
        try { DocumentCodec.Encode(new Document { ["blob"] = new byte[] { 0 } }); }
        catch (ProtocolException) { threw = true; }
        Check(threw, "document rejects binary fields");
    }

    private static void QueryEncodes()
    {
        var q = Q.And(Q.Eq("city", "NYC"), Q.Gt("age", 30L), Q.In("tag", new object?[] { "a", "b" }));
        var blob = QueryCodec.Encode(q);
        Check(blob[0] == 10, "query AND discriminant");
    }

    private static void UpdateCountPrefix()
    {
        var ops = new List<DocUpdate> { U.Set("city", "Boston"), U.Inc("age", 1), U.Unset("temp") };
        var blob = UpdateCodec.Encode(ops);
        long count = blob[0] | ((long)blob[1] << 8) | ((long)blob[2] << 16) | ((long)blob[3] << 24);
        Check(count == 3, "update count prefix");
    }

    private static void SqlPacketHeader()
    {
        var (t, b) = Protocol.SqlBody("SELECT 1", Array.Empty<object?>(), 1);
        var packet = Protocol.EncodePacket(7, t, b);
        long reqId = packet[4] | ((long)packet[5] << 8) | ((long)packet[6] << 16) | ((long)packet[7] << 24);
        Check(packet[0] == 0x20 && reqId == 7, "sql packet header + request id");
    }

    private static void DecodeAuthAck()
    {
        var w = new Writer();
        w.U8(0x04);            // AuthAck
        w.Raw(new byte[3]);
        w.U32(99);             // request id
        w.Raw(new byte[4]);
        w.U8(0);               // status OK
        w.U64(1234);           // user_oid
        var packet = Protocol.DecodePacket(w.Out());
        Check(packet.RequestId == 99, "authack request id");
        Check(((AuthAckMsg)packet.Message).UserOid == 1234, "authack user_oid");
    }

    private static void DecodeErrorTrailer()
    {
        var w = new Writer();
        w.U8(0x13);            // TxnAck
        w.Raw(new byte[3]);
        w.U32(1);
        w.Raw(new byte[4]);
        w.U8(2);               // status != 0
        w.U64(0);              // txn_id
        w.U64(0);              // commit_lsn
        w.U32(0x0201);
        w.StrU16("serialization failure");
        w.Raw(Encoding.ASCII.GetBytes("40001"));
        w.StrU16("");
        w.U32(0);
        var packet = Protocol.DecodePacket(w.Out());
        var err = packet.Message.Error;
        Check(err != null && err.Code == 0x0201 && err.SqlState == "40001", "error trailer decode");
    }
}
