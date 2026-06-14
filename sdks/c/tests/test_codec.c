/*
 * No-server codec round-trip tests for the C SDK.
 *
 * Includes prism.c directly so it can exercise the static codec internals
 * (buffer, reader, value/document/query/update encoders). Builds and runs
 * offline; exits 0 if all checks pass, 1 otherwise.
 *
 *   cc -I../include test_codec.c -o test_codec        (POSIX)
 *   gcc -I include tests/test_codec.c -lws2_32 -o t    (Windows/MinGW)
 */
#include "../src/prism.c"

#include <stdio.h>

static int failures = 0;
static void check(int cond, const char *name) {
    printf("%s %s\n", cond ? "ok  " : "FAIL", name);
    if (!cond) failures++;
}

static void test_buf_rd(void) {
    buf b = {0};
    bw_u8(&b, 0x7f);
    bw_u16(&b, 0xBEEF);
    bw_u32(&b, 0xDEADBEEFu);
    bw_i32(&b, -5);
    bw_u64(&b, 0x1122334455667788ull);
    bw_i64(&b, -9000000000ll);
    bw_f64(&b, 3.5);
    bw_str16(&b, "hi", 2);
    check(!b.oom, "buffer no oom");

    rd r = { b.data, b.len, 0, 0 };
    check(rd_u8(&r) == 0x7f, "u8 roundtrip");
    check(rd_u16(&r) == 0xBEEF, "u16 roundtrip");
    check(rd_u32(&r) == 0xDEADBEEFu, "u32 roundtrip");
    check(rd_i32(&r) == -5, "i32 roundtrip");
    check(rd_u64(&r) == 0x1122334455667788ull, "u64 roundtrip");
    check(rd_i64(&r) == -9000000000ll, "i64 roundtrip");
    check(rd_f64(&r) == 3.5, "f64 roundtrip");
    uint16_t n = rd_u16(&r);
    const uint8_t *s = rd_raw(&r, n);
    check(n == 2 && s && s[0] == 'h' && s[1] == 'i', "str16 roundtrip");
    check(!r.err, "reader no error");
    free(b.data);
}

static void test_truncation(void) {
    uint8_t bytes[2] = { 1, 2 };
    rd r = { bytes, 2, 0, 0 };
    rd_u32(&r);
    check(r.err == 1, "reader flags truncation");
}

static void test_value_roundtrip(void) {
    prism_value cases[] = {
        prism_null(), prism_bool(1), prism_bool(0), prism_i64(42), prism_i64(-7),
        prism_i32(123), prism_f64(3.25), prism_str("prism"),
        prism_timestamp(1700000000000000ll),
    };
    for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
        buf b = {0};
        enc_tagged(&b, &cases[i]);
        rd r = { b.data, b.len, 0, 0 };
        uint8_t tag = rd_u8(&r);
        prism_value got = dec_untagged(&r, tag);
        int ok = !r.err && got.tag == cases[i].tag;
        if (ok && tag == PRISM_TAG_INT64) ok = got.as.i64 == cases[i].as.i64;
        if (ok && tag == PRISM_TAG_INT32) ok = got.as.i32 == cases[i].as.i32;
        if (ok && tag == PRISM_TAG_DOUBLE) ok = got.as.f64 == cases[i].as.f64;
        if (ok && tag == PRISM_TAG_BOOL) ok = got.as.b == cases[i].as.b;
        if (ok && tag == PRISM_TAG_TIMESTAMP) ok = got.as.i64 == cases[i].as.i64;
        if (ok && tag == PRISM_TAG_STRING)
            ok = got.as.str.len == cases[i].as.str.len &&
                 memcmp(got.as.str.ptr, cases[i].as.str.ptr, got.as.str.len) == 0;
        check(ok, "value tagged roundtrip");
        value_free(&got);
        free(b.data);
    }
}

static void test_int_default_tag(void) {
    prism_value v = prism_i64(1);
    buf b = {0};
    enc_tagged(&b, &v);
    check(b.data[0] == PRISM_TAG_INT64, "i64 helper tags Int64");
    free(b.data);
}

static void test_objectid(void) {
    uint8_t oid[12];
    check(prism_objectid_from_hex("507f1f77bcf86cd799439011", oid) == PRISM_OK, "objectid from hex");
    char hex[25];
    prism_objectid_to_hex(oid, hex);
    check(strcmp(hex, "507f1f77bcf86cd799439011") == 0, "objectid to hex");
    check(prism_objectid_from_hex("xyz", oid) == PRISM_ERR_USAGE, "objectid rejects bad hex");
}

static void test_document(void) {
    prism_doc *d = prism_doc_new();
    prism_doc_set(d, "name", prism_str("carol"));
    prism_doc_set(d, "age", prism_i64(41));
    prism_doc_set(d, "active", prism_bool(1));
    buf out = {0};
    check(doc_encode(d, &out) == 0, "document encodes");
    uint32_t total = out.data[0] | (out.data[1] << 8) | (out.data[2] << 16) | ((uint32_t)out.data[3] << 24);
    check(total == out.len, "document total-length prefix");
    prism_doc *back = doc_decode(out.data, out.len);
    int found = 0;
    prism_value age = prism_doc_get(back, "age", &found);
    check(found && age.tag == PRISM_TAG_INT64 && age.as.i64 == 41, "document roundtrip age");
    prism_value name = prism_doc_get(back, "name", &found);
    check(found && name.tag == PRISM_TAG_STRING && strcmp(name.as.str.ptr, "carol") == 0, "document roundtrip name");
    prism_doc_free(back);
    free(out.data);

    /* binary fields are rejected in documents */
    prism_doc *d2 = prism_doc_new();
    uint8_t raw[1] = { 0 };
    prism_doc_set(d2, "blob", prism_bytes(raw, 1));
    buf out2 = {0};
    check(doc_encode(d2, &out2) != 0, "document rejects binary fields");
    free(out2.data);
    prism_doc_free(d2);
    prism_doc_free(d);
}

static void test_query_update(void) {
    prism_query *subs[2] = { prism_q_eq("city", prism_str("NYC")), prism_q_gt("age", prism_i64(30)) };
    prism_query *q = prism_q_and(subs, 2);
    buf b = {0};
    q_encode(&b, q);
    check(b.data[0] == 10, "query AND discriminant");
    free(b.data);
    prism_query_free(q);

    prism_update *u = prism_update_new();
    prism_update_set(u, "city", prism_str("Boston"));
    prism_update_inc(u, "age", 1);
    prism_update_unset(u, "temp");
    buf ub = {0};
    update_encode(&ub, u);
    uint32_t count = ub.data[0] | (ub.data[1] << 8) | (ub.data[2] << 16) | ((uint32_t)ub.data[3] << 24);
    check(count == 3, "update count prefix");
    free(ub.data);
    prism_update_free(u);
}

static void test_trailer(void) {
    /* Build a TxnAck-style body with an error trailer and decode it. */
    buf b = {0};
    bw_u8(&b, 2);          /* status != 0 */
    bw_u32(&b, 0x0201);    /* code */
    bw_str16(&b, "serialization failure", 21);
    bw_raw(&b, "40001", 5);
    bw_str16(&b, "", 0);
    bw_u32(&b, 0);
    prism_client c;
    memset(&c, 0, sizeof(c));
    rd r = { b.data, b.len, 0, 0 };
    uint8_t status = rd_u8(&r);
    prism_status st = read_trailer(&c, &r, status);
    check(st == PRISM_ERR_SERVER, "trailer yields server error");
    check(c.last_error.code == 0x0201, "trailer code");
    check(strcmp(c.last_error.sqlstate, "40001") == 0, "trailer sqlstate");
    free(b.data);
}

int main(void) {
    test_buf_rd();
    test_truncation();
    test_value_roundtrip();
    test_int_default_tag();
    test_objectid();
    test_document();
    test_query_update();
    test_trailer();
    printf("\n%s\n", failures == 0 ? "ALL TESTS PASSED" : "SOME TESTS FAILED");
    return failures == 0 ? 1 - 1 : 1;
}
