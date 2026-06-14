// No-server codec round-trip tests for the C++ SDK. Header-only: needs neither
// prism.cpp nor a socket. Exits 0 if all checks pass, 1 otherwise.
//
//   g++ -std=c++17 -Iinclude tests/test_codec.cpp -o test_codec

#include "prism/prism.hpp"

#include <cstdio>

using namespace prism;

static int failures = 0;
static void check(bool cond, const char* name) {
    std::printf("%s %s\n", cond ? "ok  " : "FAIL", name);
    if (!cond) failures++;
}

static void testWriterReader() {
    detail::Writer w;
    w.u8(0x7f);
    w.u16(0xBEEF);
    w.u32(0xDEADBEEFu);
    w.i32(-5);
    w.u64(0x1122334455667788ull);
    w.i64(-9000000000ll);
    w.f64(3.5);
    w.strU16("héllo");
    w.bytesU32({0, 1, 2});

    detail::Reader r(w.out());
    check(r.u8() == 0x7f, "u8 roundtrip");
    check(r.u16() == 0xBEEF, "u16 roundtrip");
    check(r.u32() == 0xDEADBEEFu, "u32 roundtrip");
    check(r.i32() == -5, "i32 roundtrip");
    check(r.u64() == 0x1122334455667788ull, "u64 roundtrip");
    check(r.i64() == -9000000000ll, "i64 roundtrip");
    check(r.f64() == 3.5, "f64 roundtrip");
    check(r.strU16() == "héllo", "strU16 roundtrip (utf-8)");
    check(r.bytesU32() == std::vector<std::uint8_t>({0, 1, 2}), "bytesU32 roundtrip");
    r.expectEnd();
}

static void testTruncation() {
    std::vector<std::uint8_t> bytes{1, 2};
    detail::Reader r(bytes);
    bool threw = false;
    try { r.u32(); } catch (const ProtocolError&) { threw = true; }
    check(threw, "reader flags truncation");
}

static void testValueRoundtrip() {
    std::vector<Value> cases = {
        Value(),                        // null
        Value(true), Value(false),
        Value(std::int64_t(42)), Value(std::int64_t(-7)),
        Value(Int32{123}),
        Value(3.25),
        Value(std::string("prism")),
        Value(std::vector<std::uint8_t>{0xDE, 0xAD}),
        Value(Timestamp{1700000000000000ll}),
        Value(ObjectId::fromHex("507f1f77bcf86cd799439011")),
    };
    for (const auto& v : cases) {
        detail::Writer w;
        detail::encodeTagged(w, v);
        detail::Reader r(w.out());
        Value got = detail::decodeTagged(r);
        bool ok = got.tag() == v.tag();
        if (ok) {
            switch (v.tag()) {
                case Tag::Bool: ok = got.asBool() == v.asBool(); break;
                case Tag::Int32: ok = got.asInt32() == v.asInt32(); break;
                case Tag::Int64:
                case Tag::Timestamp: ok = got.asInt64() == v.asInt64(); break;
                case Tag::Double: ok = got.asDouble() == v.asDouble(); break;
                case Tag::String: ok = got.asString() == v.asString(); break;
                case Tag::Binary: ok = got.asBytes() == v.asBytes(); break;
                case Tag::ObjectId: ok = got.asObjectId() == v.asObjectId(); break;
                case Tag::Null: break;
            }
        }
        check(ok, "value tagged roundtrip");
    }
}

static void testIntDefault() {
    check(Value(1).tag() == Tag::Int64, "int literal maps to Int64");
    check(Value(Int32{1}).tag() == Tag::Int32, "Int32 wrapper maps to Int32");
}

static void testObjectId() {
    auto oid = ObjectId::fromHex("507f1f77bcf86cd799439011");
    check(oid.toHex() == "507f1f77bcf86cd799439011", "objectid hex roundtrip");
    check(oid == ObjectId(oid.bytes()), "objectid equality");
}

static void testDocument() {
    Document d{{"name", "carol"}, {"age", std::int64_t(41)}, {"active", true}, {"score", 9.5}};
    auto enc = d.encode();
    std::uint32_t total = enc[0] | (enc[1] << 8) | (enc[2] << 16) | (std::uint32_t(enc[3]) << 24);
    check(total == enc.size(), "document total-length prefix");
    Document back = Document::decode(enc.data(), enc.size());
    check(back.at("name").asString() == "carol" && back.at("age").asInt64() == 41, "document roundtrip");

    bool threw = false;
    try { Document{{"blob", std::vector<std::uint8_t>{0}}}.encode(); }
    catch (const ProtocolError&) { threw = true; }
    check(threw, "document rejects binary fields");
}

static void testQueryUpdate() {
    auto q = Query::and_(Query::eq("city", "NYC"), Query::gt("age", std::int64_t(30)),
                         Query::in("tag", {"a", "b"}));
    check(q.encode()[0] == 10, "query AND discriminant");

    Update u;
    u.set("city", "Boston").inc("age", 1).unset("temp");
    auto blob = u.encode();
    std::uint32_t count = blob[0] | (blob[1] << 8) | (blob[2] << 16) | (std::uint32_t(blob[3]) << 24);
    check(count == 3, "update count prefix");
}

static void testMessages() {
    detail::Writer body;
    body.strU32("SELECT 1");
    body.u16(0);
    body.u32(1);
    auto packet = detail::encodePacket(7, detail::msg::SqlExecute, body.out());
    check(packet[0] == detail::msg::SqlExecute, "sql packet message type");
    std::uint32_t reqId = packet[4] | (packet[5] << 8) | (packet[6] << 16) | (std::uint32_t(packet[7]) << 24);
    check(reqId == 7, "sql packet request id");

    // Build a TxnAck with an error trailer and decode it.
    detail::Writer tw;
    tw.u8(detail::msg::TxnAck);
    tw.u8(0); tw.u8(0); tw.u8(0);
    tw.u32(1);
    tw.u32(0);
    tw.u8(2);          // status != 0
    tw.u64(0);         // txn id
    tw.u64(0);         // commit lsn
    tw.u32(0x0201);
    tw.strU16("serialization failure");
    tw.raw("40001", 5);
    tw.strU16("");
    tw.u32(0);
    detail::Reader r(tw.out());
    auto h = detail::readHeader(r);
    std::uint8_t status = r.u8();
    r.u64();
    r.u64();
    auto err = detail::readTrailer(r, status);
    check(h.requestId == 1 && err && err->code == 0x0201 && err->sqlstate == "40001",
          "error trailer decode");
}

int main() {
    testWriterReader();
    testTruncation();
    testValueRoundtrip();
    testIntDefault();
    testObjectId();
    testDocument();
    testQueryUpdate();
    testMessages();
    std::printf("\n%s\n", failures == 0 ? "ALL TESTS PASSED" : "SOME TESTS FAILED");
    return failures == 0 ? 0 : 1;
}
