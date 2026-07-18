/*
 * prism.c - implementation of the pure-C PrismDB client (see include/prism.h).
 *
 * Mirrors the reference Node SDK and crates/prism-protocol byte-for-byte. All
 * multi-byte integers are little-endian; 64-bit values are written/read byte by
 * byte so the codec is independent of the host's native byte order.
 */
#include "prism.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#  ifndef WIN32_LEAN_AND_MEAN
#    define WIN32_LEAN_AND_MEAN
#  endif
#  include <winsock2.h>
#  include <ws2tcpip.h>
   typedef SOCKET sock_t;
#  define PRISM_INVALID_SOCK INVALID_SOCKET
#  define sock_close closesocket
#  define sock_errno WSAGetLastError()
#  define PRISM_EINPROGRESS WSAEWOULDBLOCK
#else
#  include <sys/types.h>
#  include <sys/socket.h>
#  include <netinet/in.h>
#  include <netinet/tcp.h>
#  include <netdb.h>
#  include <unistd.h>
#  include <fcntl.h>
#  include <errno.h>
#  include <sys/select.h>
   typedef int sock_t;
#  define PRISM_INVALID_SOCK (-1)
#  define sock_close close
#  define sock_errno errno
#  define PRISM_EINPROGRESS EINPROGRESS
#endif

/* ---- message types ------------------------------------------------------ */

#define MSG_HELLO 0x01
#define MSG_HELLO_ACK 0x02
#define MSG_AUTH 0x03
#define MSG_AUTH_ACK 0x04
#define MSG_BEGIN 0x10
#define MSG_COMMIT 0x11
#define MSG_ABORT 0x12
#define MSG_TXN_ACK 0x13
#define MSG_SQL_EXECUTE 0x20
#define MSG_SQL_RESULT 0x21
#define MSG_DOC_OP 0x30
#define MSG_DOC_RESULT 0x31
#define MSG_KV_OP 0x40
#define MSG_KV_RESULT 0x41
#define MSG_NOTICE 0x60
#define MSG_PING 0x70
#define MSG_PONG 0x71

#define AUTH_PASSWORD 1
#define FEATURE_CONNECT_DB 1u
#define PROTOCOL_VERSION 1
#define MAX_FRAME (64u * 1024u * 1024u)

/* ---- growable write buffer ---------------------------------------------- */

typedef struct {
    uint8_t *data;
    size_t len, cap;
    int oom;
} buf;

static int buf_ensure(buf *b, size_t extra) {
    if (b->oom) return -1;
    if (b->len + extra <= b->cap) return 0;
    size_t cap = b->cap ? b->cap * 2 : 64;
    while (cap < b->len + extra) cap *= 2;
    uint8_t *p = (uint8_t *)realloc(b->data, cap);
    if (!p) { b->oom = 1; return -1; }
    b->data = p;
    b->cap = cap;
    return 0;
}
static void bw_u8(buf *b, uint8_t v) { if (!buf_ensure(b, 1)) b->data[b->len++] = v; }
static void bw_u16(buf *b, uint16_t v) {
    if (buf_ensure(b, 2)) return;
    b->data[b->len++] = (uint8_t)(v & 0xff);
    b->data[b->len++] = (uint8_t)((v >> 8) & 0xff);
}
static void bw_u32(buf *b, uint32_t v) {
    if (buf_ensure(b, 4)) return;
    for (int i = 0; i < 4; i++) b->data[b->len++] = (uint8_t)((v >> (8 * i)) & 0xff);
}
static void bw_u64(buf *b, uint64_t v) {
    if (buf_ensure(b, 8)) return;
    for (int i = 0; i < 8; i++) b->data[b->len++] = (uint8_t)((v >> (8 * i)) & 0xff);
}
static void bw_i32(buf *b, int32_t v) { bw_u32(b, (uint32_t)v); }
static void bw_i64(buf *b, int64_t v) { bw_u64(b, (uint64_t)v); }
static void bw_f64(buf *b, double d) { uint64_t u; memcpy(&u, &d, 8); bw_u64(b, u); }
static void bw_u128(buf *b, uint64_t lo) { bw_u64(b, lo); bw_u64(b, 0); }
static void bw_raw(buf *b, const void *p, size_t n) {
    if (buf_ensure(b, n)) return;
    if (n) memcpy(b->data + b->len, p, n);
    b->len += n;
}
static void bw_str16(buf *b, const char *s, size_t n) { bw_u16(b, (uint16_t)n); bw_raw(b, s, n); }
static void bw_str32(buf *b, const char *s, size_t n) { bw_u32(b, (uint32_t)n); bw_raw(b, s, n); }
static void bw_bytes16(buf *b, const void *p, size_t n) { bw_u16(b, (uint16_t)n); bw_raw(b, p, n); }
static void bw_bytes32(buf *b, const void *p, size_t n) { bw_u32(b, (uint32_t)n); bw_raw(b, p, n); }

/* ---- bounds-checked reader ---------------------------------------------- */

typedef struct {
    const uint8_t *p;
    size_t len, off;
    int err;
} rd;

static int rd_need(rd *r, size_t n) {
    if (r->err) return -1;
    if (r->off + n > r->len) { r->err = 1; return -1; }
    return 0;
}
static uint8_t rd_u8(rd *r) { if (rd_need(r, 1)) return 0; return r->p[r->off++]; }
static uint16_t rd_u16(rd *r) {
    if (rd_need(r, 2)) return 0;
    uint16_t v = (uint16_t)(r->p[r->off] | (r->p[r->off + 1] << 8));
    r->off += 2;
    return v;
}
static uint32_t rd_u32(rd *r) {
    if (rd_need(r, 4)) return 0;
    uint32_t v = 0;
    for (int i = 0; i < 4; i++) v |= (uint32_t)r->p[r->off + i] << (8 * i);
    r->off += 4;
    return v;
}
static uint64_t rd_u64(rd *r) {
    if (rd_need(r, 8)) return 0;
    uint64_t v = 0;
    for (int i = 0; i < 8; i++) v |= (uint64_t)r->p[r->off + i] << (8 * i);
    r->off += 8;
    return v;
}
static int32_t rd_i32(rd *r) { return (int32_t)rd_u32(r); }
static int64_t rd_i64(rd *r) { return (int64_t)rd_u64(r); }
static double rd_f64(rd *r) { uint64_t u = rd_u64(r); double d; memcpy(&d, &u, 8); return d; }
static const uint8_t *rd_raw(rd *r, size_t n) {
    if (rd_need(r, n)) return NULL;
    const uint8_t *p = r->p + r->off;
    r->off += n;
    return p;
}

/* ---- small helpers ------------------------------------------------------ */

static void *dup_mem(const void *p, size_t n) {
    uint8_t *q = (uint8_t *)malloc(n + 1);
    if (!q) return NULL;
    if (n) memcpy(q, p, n);
    q[n] = 0; /* convenience NUL for strings */
    return q;
}
static char *dup_str(const char *s) { return (char *)dup_mem(s, strlen(s)); }
static void copy_bounded(char *dst, size_t dstsz, const uint8_t *src, size_t n) {
    if (!src || dstsz == 0) { if (dstsz) dst[0] = 0; return; }
    size_t k = n < dstsz - 1 ? n : dstsz - 1;
    memcpy(dst, src, k);
    dst[k] = 0;
}

/* ---- value construction ------------------------------------------------- */

prism_value prism_null(void) { prism_value v; v.tag = PRISM_TAG_NULL; return v; }
prism_value prism_bool(int b) { prism_value v; v.tag = PRISM_TAG_BOOL; v.as.b = b ? 1 : 0; return v; }
prism_value prism_i32(int32_t x) { prism_value v; v.tag = PRISM_TAG_INT32; v.as.i32 = x; return v; }
prism_value prism_i64(int64_t x) { prism_value v; v.tag = PRISM_TAG_INT64; v.as.i64 = x; return v; }
prism_value prism_f64(double x) { prism_value v; v.tag = PRISM_TAG_DOUBLE; v.as.f64 = x; return v; }
prism_value prism_timestamp(int64_t us) { prism_value v; v.tag = PRISM_TAG_TIMESTAMP; v.as.i64 = us; return v; }
prism_value prism_strn(const char *s, size_t n) {
    prism_value v; v.tag = PRISM_TAG_STRING; v.as.str.ptr = s; v.as.str.len = n; return v;
}
prism_value prism_str(const char *s) { return prism_strn(s, s ? strlen(s) : 0); }
prism_value prism_bytes(const uint8_t *p, size_t n) {
    prism_value v; v.tag = PRISM_TAG_BINARY; v.as.bin.ptr = p; v.as.bin.len = n; return v;
}
prism_value prism_objectid(const uint8_t oid[12]) {
    prism_value v; v.tag = PRISM_TAG_OBJECTID; memcpy(v.as.oid, oid, 12); return v;
}

void prism_objectid_to_hex(const uint8_t oid[12], char hex[25]) {
    static const char *H = "0123456789abcdef";
    for (int i = 0; i < 12; i++) { hex[i * 2] = H[oid[i] >> 4]; hex[i * 2 + 1] = H[oid[i] & 0xf]; }
    hex[24] = 0;
}
static int hexval(char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}
prism_status prism_objectid_from_hex(const char *hex, uint8_t oid[12]) {
    if (!hex || strlen(hex) != 24) return PRISM_ERR_USAGE;
    for (int i = 0; i < 12; i++) {
        int hi = hexval(hex[i * 2]), lo = hexval(hex[i * 2 + 1]);
        if (hi < 0 || lo < 0) return PRISM_ERR_USAGE;
        oid[i] = (uint8_t)((hi << 4) | lo);
    }
    return PRISM_OK;
}

/* Deep-copy a value's heap data so it can outlive the caller's buffers. */
static int value_own(prism_value *v) {
    if (v->tag == PRISM_TAG_STRING && v->as.str.len) {
        void *p = dup_mem(v->as.str.ptr, v->as.str.len);
        if (!p) return -1;
        v->as.str.ptr = (const char *)p;
    } else if (v->tag == PRISM_TAG_BINARY && v->as.bin.len) {
        void *p = dup_mem(v->as.bin.ptr, v->as.bin.len);
        if (!p) return -1;
        v->as.bin.ptr = (const uint8_t *)p;
    }
    return 0;
}
static void value_free(prism_value *v) {
    if (v->tag == PRISM_TAG_STRING && v->as.str.len) free((void *)v->as.str.ptr);
    else if (v->tag == PRISM_TAG_BINARY && v->as.bin.len) free((void *)v->as.bin.ptr);
    v->tag = PRISM_TAG_NULL;
}

/* ---- value codec -------------------------------------------------------- */

static void enc_untagged(buf *b, int tag, const prism_value *v) {
    switch (tag) {
        case PRISM_TAG_NULL: break;
        case PRISM_TAG_BOOL: bw_u8(b, v->as.b ? 1 : 0); break;
        case PRISM_TAG_INT32: bw_i32(b, v->as.i32); break;
        case PRISM_TAG_INT64: bw_i64(b, v->as.i64); break;
        case PRISM_TAG_DOUBLE: bw_f64(b, v->as.f64); break;
        case PRISM_TAG_TIMESTAMP: bw_i64(b, v->as.i64); break;
        case PRISM_TAG_STRING: bw_str32(b, v->as.str.ptr, v->as.str.len); break;
        case PRISM_TAG_OBJECTID: bw_raw(b, v->as.oid, 12); break;
        case PRISM_TAG_BINARY:
            bw_u32(b, (uint32_t)v->as.bin.len);
            bw_u8(b, 0); /* subtype */
            bw_raw(b, v->as.bin.ptr, v->as.bin.len);
            break;
        default: b->oom = 1; break; /* surfaced as encode failure */
    }
}
static void enc_tagged(buf *b, const prism_value *v) {
    bw_u8(b, (uint8_t)v->tag);
    enc_untagged(b, v->tag, v);
}

/* Decode an untagged value, owning any heap data. */
static prism_value dec_untagged(rd *r, int tag) {
    prism_value v;
    v.tag = (prism_tag)tag;
    switch (tag) {
        case PRISM_TAG_NULL: break;
        case PRISM_TAG_BOOL: v.as.b = rd_u8(r) != 0; break;
        case PRISM_TAG_INT32: v.as.i32 = rd_i32(r); break;
        case PRISM_TAG_INT64: v.as.i64 = rd_i64(r); break;
        case PRISM_TAG_DOUBLE: v.as.f64 = rd_f64(r); break;
        case PRISM_TAG_TIMESTAMP: v.as.i64 = rd_i64(r); break;
        case PRISM_TAG_STRING: {
            uint32_t n = rd_u32(r);
            const uint8_t *p = rd_raw(r, n);
            v.as.str.len = n;
            v.as.str.ptr = p ? (const char *)dup_mem(p, n) : NULL;
            break;
        }
        case PRISM_TAG_BINARY: {
            uint32_t n = rd_u32(r);
            rd_u8(r); /* subtype */
            const uint8_t *p = rd_raw(r, n);
            v.as.bin.len = n;
            v.as.bin.ptr = p ? (const uint8_t *)dup_mem(p, n) : NULL;
            break;
        }
        case PRISM_TAG_OBJECTID: {
            const uint8_t *p = rd_raw(r, 12);
            if (p) memcpy(v.as.oid, p, 12); else memset(v.as.oid, 0, 12);
            break;
        }
        default: v.tag = PRISM_TAG_NULL; r->err = 1; break;
    }
    return v;
}

/* ---- document ----------------------------------------------------------- */

struct prism_doc {
    char **names;
    prism_value *vals;
    size_t len, cap;
};

prism_doc *prism_doc_new(void) { return (prism_doc *)calloc(1, sizeof(prism_doc)); }
void prism_doc_free(prism_doc *d) {
    if (!d) return;
    for (size_t i = 0; i < d->len; i++) { free(d->names[i]); value_free(&d->vals[i]); }
    free(d->names);
    free(d->vals);
    free(d);
}
static int doc_grow(prism_doc *d) {
    if (d->len < d->cap) return 0;
    size_t cap = d->cap ? d->cap * 2 : 8;
    char **n = (char **)realloc(d->names, cap * sizeof(char *));
    if (!n) return -1;
    d->names = n;
    prism_value *v = (prism_value *)realloc(d->vals, cap * sizeof(prism_value));
    if (!v) return -1;
    d->vals = v;
    d->cap = cap;
    return 0;
}
prism_status prism_doc_set(prism_doc *d, const char *field, prism_value v) {
    if (!d || !field) return PRISM_ERR_USAGE;
    if (doc_grow(d)) return PRISM_ERR_OOM;
    char *name = dup_str(field);
    if (!name) return PRISM_ERR_OOM;
    prism_value owned = v;
    if (value_own(&owned)) { free(name); return PRISM_ERR_OOM; }
    d->names[d->len] = name;
    d->vals[d->len] = owned;
    d->len++;
    return PRISM_OK;
}
size_t prism_doc_len(const prism_doc *d) { return d ? d->len : 0; }
const char *prism_doc_field_name(const prism_doc *d, size_t i) {
    return (d && i < d->len) ? d->names[i] : NULL;
}
prism_value prism_doc_at(const prism_doc *d, size_t i) {
    if (d && i < d->len) return d->vals[i];
    return prism_null();
}
prism_value prism_doc_get(const prism_doc *d, const char *field, int *found) {
    if (d && field) {
        for (size_t i = 0; i < d->len; i++) {
            if (strcmp(d->names[i], field) == 0) { if (found) *found = 1; return d->vals[i]; }
        }
    }
    if (found) *found = 0;
    return prism_null();
}

static int doc_encode(const prism_doc *d, buf *out) {
    buf body = {0};
    bw_u16(&body, (uint16_t)d->len);
    for (size_t i = 0; i < d->len; i++) {
        int tag = d->vals[i].tag;
        if (tag == PRISM_TAG_BINARY) { free(body.data); return -1; } /* not allowed in docs */
        bw_u8(&body, (uint8_t)tag);
        bw_str16(&body, d->names[i], strlen(d->names[i]));
        enc_untagged(&body, tag, &d->vals[i]);
    }
    if (body.oom) { free(body.data); return -1; }
    bw_u32(out, (uint32_t)(4 + body.len));
    bw_raw(out, body.data, body.len);
    free(body.data);
    return out->oom ? -1 : 0;
}

static prism_doc *doc_decode(const uint8_t *p, size_t n) {
    rd r = { p, n, 0, 0 };
    rd_u32(&r); /* total length */
    uint16_t count = rd_u16(&r);
    prism_doc *d = prism_doc_new();
    if (!d) return NULL;
    for (uint16_t i = 0; i < count; i++) {
        uint8_t tag = rd_u8(&r);
        uint16_t nlen = rd_u16(&r);
        const uint8_t *np = rd_raw(&r, nlen);
        prism_value v = dec_untagged(&r, tag);
        if (r.err || !np) { value_free(&v); prism_doc_free(d); return NULL; }
        if (doc_grow(d)) { value_free(&v); prism_doc_free(d); return NULL; }
        d->names[d->len] = (char *)dup_mem(np, nlen);
        if (!d->names[d->len]) { value_free(&v); prism_doc_free(d); return NULL; }
        d->vals[d->len] = v;
        d->len++;
    }
    if (r.err) { prism_doc_free(d); return NULL; }
    return d;
}

/* ---- query -------------------------------------------------------------- */

enum { Q_ALL, Q_FIELD, Q_SET, Q_EXISTS, Q_GROUP, Q_NOT };
enum {
    QT_ALL = 0, QT_EQ = 1, QT_NE = 2, QT_GT = 3, QT_LT = 4, QT_GTE = 5, QT_LTE = 6,
    QT_IN = 7, QT_NIN = 8, QT_EXISTS = 9, QT_AND = 10, QT_OR = 11, QT_NOT = 12
};

struct prism_query {
    int kind;
    int tag;
    char *field;
    prism_value value;
    prism_value *values;
    size_t nvalues;
    int present;
    prism_query **subs;
    size_t nsubs;
    prism_query *sub;
};

void prism_query_free(prism_query *q) {
    if (!q) return;
    free(q->field);
    value_free(&q->value);
    for (size_t i = 0; i < q->nvalues; i++) value_free(&q->values[i]);
    free(q->values);
    for (size_t i = 0; i < q->nsubs; i++) prism_query_free(q->subs[i]);
    free(q->subs);
    prism_query_free(q->sub);
    free(q);
}
static prism_query *q_new(int kind, int tag) {
    prism_query *q = (prism_query *)calloc(1, sizeof(prism_query));
    if (q) { q->kind = kind; q->tag = tag; q->value.tag = PRISM_TAG_NULL; }
    return q;
}
prism_query *prism_q_all(void) { return q_new(Q_ALL, QT_ALL); }
static prism_query *q_field(int tag, const char *field, prism_value v) {
    prism_query *q = q_new(Q_FIELD, tag);
    if (!q) return NULL;
    q->field = dup_str(field);
    q->value = v;
    if (!q->field || value_own(&q->value)) { prism_query_free(q); return NULL; }
    return q;
}
prism_query *prism_q_eq(const char *f, prism_value v) { return q_field(QT_EQ, f, v); }
prism_query *prism_q_ne(const char *f, prism_value v) { return q_field(QT_NE, f, v); }
prism_query *prism_q_gt(const char *f, prism_value v) { return q_field(QT_GT, f, v); }
prism_query *prism_q_lt(const char *f, prism_value v) { return q_field(QT_LT, f, v); }
prism_query *prism_q_gte(const char *f, prism_value v) { return q_field(QT_GTE, f, v); }
prism_query *prism_q_lte(const char *f, prism_value v) { return q_field(QT_LTE, f, v); }
static prism_query *q_set(int tag, const char *field, const prism_value *vals, size_t n) {
    prism_query *q = q_new(Q_SET, tag);
    if (!q) return NULL;
    q->field = dup_str(field);
    if (!q->field) { prism_query_free(q); return NULL; }
    if (n) {
        q->values = (prism_value *)calloc(n, sizeof(prism_value));
        if (!q->values) { prism_query_free(q); return NULL; }
        for (size_t i = 0; i < n; i++) {
            q->values[i] = vals[i];
            if (value_own(&q->values[i])) { q->nvalues = i; prism_query_free(q); return NULL; }
        }
        q->nvalues = n;
    }
    return q;
}
prism_query *prism_q_in(const char *f, const prism_value *v, size_t n) { return q_set(QT_IN, f, v, n); }
prism_query *prism_q_nin(const char *f, const prism_value *v, size_t n) { return q_set(QT_NIN, f, v, n); }
prism_query *prism_q_exists(const char *field, int present) {
    prism_query *q = q_new(Q_EXISTS, QT_EXISTS);
    if (!q) return NULL;
    q->field = dup_str(field);
    q->present = present ? 1 : 0;
    if (!q->field) { prism_query_free(q); return NULL; }
    return q;
}
static prism_query *q_group(int tag, prism_query **subs, size_t n) {
    prism_query *q = q_new(Q_GROUP, tag);
    if (!q) { for (size_t i = 0; i < n; i++) prism_query_free(subs[i]); return NULL; }
    if (n) {
        q->subs = (prism_query **)calloc(n, sizeof(prism_query *));
        if (!q->subs) { for (size_t i = 0; i < n; i++) prism_query_free(subs[i]); free(q); return NULL; }
        for (size_t i = 0; i < n; i++) q->subs[i] = subs[i];
        q->nsubs = n;
    }
    return q;
}
prism_query *prism_q_and(prism_query **subs, size_t n) { return q_group(QT_AND, subs, n); }
prism_query *prism_q_or(prism_query **subs, size_t n) { return q_group(QT_OR, subs, n); }
prism_query *prism_q_not(prism_query *sub) {
    prism_query *q = q_new(Q_NOT, QT_NOT);
    if (!q) { prism_query_free(sub); return NULL; }
    q->sub = sub;
    return q;
}

static void q_encode(buf *b, const prism_query *q) {
    switch (q->kind) {
        case Q_ALL: bw_u8(b, QT_ALL); break;
        case Q_FIELD:
            bw_u8(b, (uint8_t)q->tag);
            bw_str16(b, q->field, strlen(q->field));
            enc_tagged(b, &q->value);
            break;
        case Q_SET:
            bw_u8(b, (uint8_t)q->tag);
            bw_str16(b, q->field, strlen(q->field));
            bw_u32(b, (uint32_t)q->nvalues);
            for (size_t i = 0; i < q->nvalues; i++) enc_tagged(b, &q->values[i]);
            break;
        case Q_EXISTS:
            bw_u8(b, QT_EXISTS);
            bw_str16(b, q->field, strlen(q->field));
            bw_u8(b, (uint8_t)q->present);
            break;
        case Q_GROUP:
            bw_u8(b, (uint8_t)q->tag);
            bw_u32(b, (uint32_t)q->nsubs);
            for (size_t i = 0; i < q->nsubs; i++) q_encode(b, q->subs[i]);
            break;
        case Q_NOT:
            bw_u8(b, QT_NOT);
            q_encode(b, q->sub);
            break;
    }
}

/* ---- update ------------------------------------------------------------- */

typedef struct { int op; char *field; prism_value value; int64_t delta; } uop;
struct prism_update { uop *ops; size_t len, cap; };

prism_update *prism_update_new(void) { return (prism_update *)calloc(1, sizeof(prism_update)); }
void prism_update_free(prism_update *u) {
    if (!u) return;
    for (size_t i = 0; i < u->len; i++) { free(u->ops[i].field); value_free(&u->ops[i].value); }
    free(u->ops);
    free(u);
}
static uop *update_push(prism_update *u, int op, const char *field) {
    if (u->len >= u->cap) {
        size_t cap = u->cap ? u->cap * 2 : 8;
        uop *p = (uop *)realloc(u->ops, cap * sizeof(uop));
        if (!p) return NULL;
        u->ops = p;
        u->cap = cap;
    }
    uop *o = &u->ops[u->len];
    memset(o, 0, sizeof(*o));
    o->op = op;
    o->value.tag = PRISM_TAG_NULL;
    o->field = dup_str(field);
    if (!o->field) return NULL;
    u->len++;
    return o;
}
prism_status prism_update_set(prism_update *u, const char *field, prism_value v) {
    if (!u || !field) return PRISM_ERR_USAGE;
    uop *o = update_push(u, 1, field);
    if (!o) return PRISM_ERR_OOM;
    o->value = v;
    if (value_own(&o->value)) return PRISM_ERR_OOM;
    return PRISM_OK;
}
prism_status prism_update_unset(prism_update *u, const char *field) {
    if (!u || !field) return PRISM_ERR_USAGE;
    return update_push(u, 2, field) ? PRISM_OK : PRISM_ERR_OOM;
}
prism_status prism_update_inc(prism_update *u, const char *field, int64_t delta) {
    if (!u || !field) return PRISM_ERR_USAGE;
    uop *o = update_push(u, 3, field);
    if (!o) return PRISM_ERR_OOM;
    o->delta = delta;
    return PRISM_OK;
}
static void update_encode(buf *b, const prism_update *u) {
    bw_u32(b, (uint32_t)(u ? u->len : 0));
    if (!u) return;
    for (size_t i = 0; i < u->len; i++) {
        uop *o = &u->ops[i];
        bw_u8(b, (uint8_t)o->op);
        bw_str16(b, o->field, strlen(o->field));
        if (o->op == 1) enc_tagged(b, &o->value);
        else if (o->op == 3) bw_i64(b, o->delta);
    }
}

/* ---- client / transport ------------------------------------------------- */

struct prism_client {
    sock_t fd;
    uint32_t next_id;
    prism_error last_error;
    int has_error;
};

prism_options prism_options_default(void) {
    prism_options o;
    memset(&o, 0, sizeof(o));
    o.host = "127.0.0.1";
    o.port = 4444;
    o.connect_timeout_ms = 10000;
    o.client_name = "prismdb-c";
    o.client_version = "0.1.0";
    return o;
}

const prism_error *prism_last_error(const prism_client *c) {
    return (c && c->has_error) ? &c->last_error : NULL;
}
const char *prism_status_str(prism_status s) {
    switch (s) {
        case PRISM_OK: return "ok";
        case PRISM_ERR_IO: return "io error";
        case PRISM_ERR_PROTOCOL: return "protocol error";
        case PRISM_ERR_SERVER: return "server error";
        case PRISM_ERR_USAGE: return "usage error";
        case PRISM_ERR_OOM: return "out of memory";
        default: return "unknown";
    }
}
void prism_free(void *p) { free(p); }

static int set_nonblocking(sock_t fd, int on) {
#ifdef _WIN32
    u_long mode = on ? 1 : 0;
    return ioctlsocket(fd, FIONBIO, &mode) == 0 ? 0 : -1;
#else
    int flags = fcntl(fd, F_GETFL, 0);
    if (flags < 0) return -1;
    flags = on ? (flags | O_NONBLOCK) : (flags & ~O_NONBLOCK);
    return fcntl(fd, F_SETFL, flags);
#endif
}

static prism_status sock_connect(const char *host, int port, int timeout_ms, sock_t *out) {
    char portstr[16];
    snprintf(portstr, sizeof(portstr), "%d", port);
    struct addrinfo hints, *res = NULL, *ai;
    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    if (getaddrinfo(host, portstr, &hints, &res) != 0) return PRISM_ERR_IO;

    prism_status rc = PRISM_ERR_IO;
    for (ai = res; ai; ai = ai->ai_next) {
        sock_t fd = socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
        if (fd == PRISM_INVALID_SOCK) continue;
        set_nonblocking(fd, 1);
        int cr = connect(fd, ai->ai_addr, (int)ai->ai_addrlen);
        int ok = 0;
        if (cr == 0) {
            ok = 1;
        } else {
            int e = sock_errno;
#ifdef _WIN32
            if (e == WSAEWOULDBLOCK || e == WSAEINPROGRESS) {
#else
            if (e == EINPROGRESS) {
#endif
                fd_set wf;
                FD_ZERO(&wf);
                FD_SET(fd, &wf);
                struct timeval tv;
                tv.tv_sec = timeout_ms / 1000;
                tv.tv_usec = (timeout_ms % 1000) * 1000;
                int sel = select((int)(fd + 1), NULL, &wf, NULL, timeout_ms > 0 ? &tv : NULL);
                if (sel > 0) {
                    int err = 0;
                    socklen_t elen = sizeof(err);
                    if (getsockopt(fd, SOL_SOCKET, SO_ERROR, (char *)&err, &elen) == 0 && err == 0) ok = 1;
                }
            }
        }
        if (ok) {
            set_nonblocking(fd, 0);
            int one = 1;
            setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, (const char *)&one, sizeof(one));
            *out = fd;
            rc = PRISM_OK;
            break;
        }
        sock_close(fd);
    }
    freeaddrinfo(res);
    return rc;
}

static int sock_write_all(sock_t fd, const uint8_t *p, size_t n) {
    size_t off = 0;
    while (off < n) {
        int k = (int)send(fd, (const char *)p + off, (int)(n - off), 0);
        if (k <= 0) return -1;
        off += (size_t)k;
    }
    return 0;
}
static int sock_read_exact(sock_t fd, uint8_t *p, size_t n) {
    size_t off = 0;
    while (off < n) {
        int k = (int)recv(fd, (char *)p + off, (int)(n - off), 0);
        if (k <= 0) return -1;
        off += (size_t)k;
    }
    return 0;
}

/* Send one client message, read frames until the matching reply arrives
 * (skipping server notices). On PRISM_OK, *payload_out (caller-freed) holds the
 * full reply payload (12-byte header + body) and *type_out its message type. */
static prism_status xchg(prism_client *c, uint8_t type, const uint8_t *body, size_t blen,
                         uint8_t **payload_out, size_t *plen_out, uint8_t *type_out) {
    uint32_t reqid = c->next_id;
    c->next_id = (c->next_id >= 0xffffffffu) ? 1 : c->next_id + 1;

    buf frame = {0};
    bw_u32(&frame, 0); /* length placeholder */
    bw_u8(&frame, type);
    bw_u8(&frame, 0); bw_u8(&frame, 0); bw_u8(&frame, 0); /* reserved */
    bw_u32(&frame, reqid);
    bw_u32(&frame, 0); /* reserved */
    if (blen) bw_raw(&frame, body, blen);
    if (frame.oom) { free(frame.data); return PRISM_ERR_OOM; }
    /* backfill payload length (frame total minus the 4-byte prefix) */
    uint32_t plen = (uint32_t)(frame.len - 4);
    for (int i = 0; i < 4; i++) frame.data[i] = (uint8_t)((plen >> (8 * i)) & 0xff);

    int w = sock_write_all(c->fd, frame.data, frame.len);
    free(frame.data);
    if (w) return PRISM_ERR_IO;

    for (;;) {
        uint8_t hdr[4];
        if (sock_read_exact(c->fd, hdr, 4)) return PRISM_ERR_IO;
        uint32_t len = (uint32_t)(hdr[0] | (hdr[1] << 8) | (hdr[2] << 16) | ((uint32_t)hdr[3] << 24));
        if (len < 12 || len > MAX_FRAME) return PRISM_ERR_PROTOCOL;
        uint8_t *payload = (uint8_t *)malloc(len);
        if (!payload) return PRISM_ERR_OOM;
        if (sock_read_exact(c->fd, payload, len)) { free(payload); return PRISM_ERR_IO; }
        uint8_t mtype = payload[0];
        uint32_t got = (uint32_t)(payload[4] | (payload[5] << 8) | (payload[6] << 16) | ((uint32_t)payload[7] << 24));
        if (mtype == MSG_NOTICE) { free(payload); continue; }
        if (got != reqid) { free(payload); continue; }
        *payload_out = payload;
        *plen_out = len;
        *type_out = mtype;
        return PRISM_OK;
    }
}

static prism_status read_trailer(prism_client *c, rd *r, uint8_t status) {
    if (status == 0) return PRISM_OK;
    prism_error *e = &c->last_error;
    memset(e, 0, sizeof(*e));
    e->code = rd_u32(r);
    uint16_t mlen = rd_u16(r);
    const uint8_t *mp = rd_raw(r, mlen);
    copy_bounded(e->message, sizeof(e->message), mp, mlen);
    const uint8_t *sp = rd_raw(r, 5);
    if (sp) { memcpy(e->sqlstate, sp, 5); e->sqlstate[5] = 0; }
    uint16_t dlen = rd_u16(r);
    const uint8_t *dp = rd_raw(r, dlen);
    copy_bounded(e->detail, sizeof(e->detail), dp, dlen);
    e->position = rd_u32(r);
    c->has_error = 1;
    return PRISM_ERR_SERVER;
}

/* ---- handshake & connect ------------------------------------------------ */

static prism_status handshake(prism_client *c, const prism_options *o, int *connect_db_honored) {
    const char *database = (o->database && o->database[0]) ? o->database : "";
    uint32_t features = database[0] ? FEATURE_CONNECT_DB : 0;

    buf b = {0};
    bw_u32(&b, PROTOCOL_VERSION);
    bw_str16(&b, o->client_name ? o->client_name : "prismdb-c", strlen(o->client_name ? o->client_name : "prismdb-c"));
    bw_str16(&b, o->client_version ? o->client_version : "0.1.0", strlen(o->client_version ? o->client_version : "0.1.0"));
    bw_u32(&b, features);
    if (features & FEATURE_CONNECT_DB) bw_str16(&b, database, strlen(database));
    if (b.oom) { free(b.data); return PRISM_ERR_OOM; }

    uint8_t *pl = NULL; size_t pn = 0; uint8_t mt = 0;
    prism_status st = xchg(c, MSG_HELLO, b.data, b.len, &pl, &pn, &mt);
    free(b.data);
    if (st) return st;
    if (mt != MSG_HELLO_ACK) { free(pl); return PRISM_ERR_PROTOCOL; }
    rd r = { pl, pn, 12, 0 };
    uint8_t status = rd_u8(&r);
    uint16_t vlen = rd_u16(&r);
    rd_raw(&r, vlen);
    uint32_t feat = rd_u32(&r);
    rd_raw(&r, 16); /* session id */
    prism_status hs = read_trailer(c, &r, status);
    free(pl);
    if (hs) return hs;
    *connect_db_honored = (feat & FEATURE_CONNECT_DB) != 0 && database[0] != 0;

    if (o->username) {
        buf a = {0};
        bw_u8(&a, AUTH_PASSWORD);
        bw_str16(&a, o->username, strlen(o->username));
        const char *pw = o->password ? o->password : "";
        bw_str16(&a, pw, strlen(pw));
        if (a.oom) { free(a.data); return PRISM_ERR_OOM; }
        st = xchg(c, MSG_AUTH, a.data, a.len, &pl, &pn, &mt);
        free(a.data);
        if (st) return st;
        if (mt != MSG_AUTH_ACK) { free(pl); return PRISM_ERR_PROTOCOL; }
        rd ar = { pl, pn, 12, 0 };
        uint8_t astatus = rd_u8(&ar);
        rd_u64(&ar); /* user_oid */
        prism_status as = read_trailer(c, &ar, astatus);
        free(pl);
        if (as) return as;
    }
    return PRISM_OK;
}

prism_status prism_connect(const prism_options *opts, prism_client **out) {
    if (!opts || !out) return PRISM_ERR_USAGE;
    if (opts->use_tls) return PRISM_ERR_USAGE; /* TLS not yet supported by the C core */
#ifdef _WIN32
    WSADATA wsa;
    if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) return PRISM_ERR_IO;
#endif
    prism_client *c = (prism_client *)calloc(1, sizeof(prism_client));
    if (!c) {
#ifdef _WIN32
        WSACleanup();
#endif
        return PRISM_ERR_OOM;
    }
    c->next_id = 1;
    const char *host = opts->host ? opts->host : "127.0.0.1";
    int port = opts->port ? opts->port : 4444;
    int tmo = opts->connect_timeout_ms ? opts->connect_timeout_ms : 10000;
    prism_status st = sock_connect(host, port, tmo, &c->fd);
    if (st) { free(c);
#ifdef _WIN32
        WSACleanup();
#endif
        return st; }

    /* fill defaults the handshake reads */
    prism_options o = *opts;
    if (!o.client_name) o.client_name = "prismdb-c";
    if (!o.client_version) o.client_version = "0.1.0";

    int honored = 0;
    st = handshake(c, &o, &honored);
    if (st) { prism_client_free(c); return st; }

    if (o.database && o.database[0] && !honored) {
        char usebuf[512];
        snprintf(usebuf, sizeof(usebuf), "USE %s", o.database);
        prism_result *res = NULL;
        st = prism_sql(c, usebuf, NULL, 0, &res);
        prism_result_free(res);
        if (st) { prism_client_free(c); return st; }
    }
    *out = c;
    return PRISM_OK;
}

void prism_client_free(prism_client *c) {
    if (!c) return;
    if (c->fd != PRISM_INVALID_SOCK) sock_close(c->fd);
    free(c);
#ifdef _WIN32
    WSACleanup();
#endif
}

prism_status prism_ping(prism_client *c) {
    if (!c) return PRISM_ERR_USAGE;
    uint8_t *pl = NULL; size_t pn = 0; uint8_t mt = 0;
    prism_status st = xchg(c, MSG_PING, NULL, 0, &pl, &pn, &mt);
    if (st) return st;
    st = (mt == MSG_PONG) ? PRISM_OK : PRISM_ERR_PROTOCOL;
    free(pl);
    return st;
}

/* ---- SQL ---------------------------------------------------------------- */

struct prism_result {
    size_t nrows, ncols;
    char **col_names;
    prism_value *cells; /* nrows * ncols */
    int64_t affected;
};

void prism_result_free(prism_result *r) {
    if (!r) return;
    for (size_t i = 0; i < r->ncols; i++) free(r->col_names[i]);
    free(r->col_names);
    if (r->cells) {
        for (size_t i = 0; i < r->nrows * r->ncols; i++) value_free(&r->cells[i]);
        free(r->cells);
    }
    free(r);
}
size_t prism_result_rows(const prism_result *r) { return r ? r->nrows : 0; }
size_t prism_result_cols(const prism_result *r) { return r ? r->ncols : 0; }
const char *prism_result_col_name(const prism_result *r, size_t col) {
    return (r && col < r->ncols) ? r->col_names[col] : NULL;
}
int64_t prism_result_affected(const prism_result *r) { return r ? r->affected : 0; }
prism_value prism_result_get(const prism_result *r, size_t row, size_t col) {
    if (r && row < r->nrows && col < r->ncols) return r->cells[row * r->ncols + col];
    return prism_null();
}

prism_status prism_sql(prism_client *c, const char *sql, const prism_value *params,
                       size_t nparams, prism_result **out) {
    if (!c || !sql || !out) return PRISM_ERR_USAGE;
    *out = NULL;
    buf b = {0};
    bw_str32(&b, sql, strlen(sql));
    bw_u16(&b, (uint16_t)nparams);
    for (size_t i = 0; i < nparams; i++) enc_tagged(&b, &params[i]);
    bw_u32(&b, 1); /* options: return_rows */
    if (b.oom) { free(b.data); return PRISM_ERR_OOM; }

    uint8_t *pl = NULL; size_t pn = 0; uint8_t mt = 0;
    prism_status st = xchg(c, MSG_SQL_EXECUTE, b.data, b.len, &pl, &pn, &mt);
    free(b.data);
    if (st) return st;
    if (mt != MSG_SQL_RESULT) { free(pl); return PRISM_ERR_PROTOCOL; }

    rd r = { pl, pn, 12, 0 };
    uint8_t status = rd_u8(&r);
    int64_t affected = (int64_t)rd_u64(&r);
    uint16_t ncols = rd_u16(&r);

    prism_result *res = (prism_result *)calloc(1, sizeof(prism_result));
    if (!res) { free(pl); return PRISM_ERR_OOM; }
    res->affected = affected;
    res->ncols = ncols;
    uint8_t *tags = NULL;
    if (ncols) {
        res->col_names = (char **)calloc(ncols, sizeof(char *));
        tags = (uint8_t *)calloc(ncols, 1);
        if (!res->col_names || !tags) { free(tags); prism_result_free(res); free(pl); return PRISM_ERR_OOM; }
        for (uint16_t i = 0; i < ncols; i++) {
            uint16_t nlen = rd_u16(&r);
            const uint8_t *np = rd_raw(&r, nlen);
            res->col_names[i] = np ? (char *)dup_mem(np, nlen) : dup_str("");
            tags[i] = rd_u8(&r);
            rd_u8(&r); /* nullable */
        }
    }
    uint32_t nrows = rd_u32(&r);
    res->nrows = nrows;
    if (nrows && ncols) {
        res->cells = (prism_value *)calloc((size_t)nrows * ncols, sizeof(prism_value));
        if (!res->cells) { free(tags); prism_result_free(res); free(pl); return PRISM_ERR_OOM; }
    }
    size_t nb = (ncols + 7) / 8;
    for (uint32_t row = 0; row < nrows; row++) {
        const uint8_t *bitmap = rd_raw(&r, nb);
        for (uint16_t col = 0; col < ncols; col++) {
            prism_value v;
            int is_null = bitmap && (bitmap[col >> 3] & (1 << (col & 7)));
            if (is_null) v = prism_null();
            else v = dec_untagged(&r, tags[col]);
            res->cells[(size_t)row * ncols + col] = v;
        }
    }
    uint8_t more = rd_u8(&r);
    prism_status hs = read_trailer(c, &r, status);
    free(tags);

    if (r.err) { prism_result_free(res); free(pl); return PRISM_ERR_PROTOCOL; }
    if (hs) { prism_result_free(res); free(pl); return hs; }
    if (more) { prism_result_free(res); free(pl); return PRISM_ERR_PROTOCOL; } /* streaming TODO */
    free(pl);
    *out = res;
    return PRISM_OK;
}

/* ---- transactions ------------------------------------------------------- */

static prism_status txn_xchg(prism_client *c, uint8_t type, const uint8_t *body, size_t blen,
                             uint64_t *txn_id_out) {
    uint8_t *pl = NULL; size_t pn = 0; uint8_t mt = 0;
    prism_status st = xchg(c, type, body, blen, &pl, &pn, &mt);
    if (st) return st;
    if (mt != MSG_TXN_ACK) { free(pl); return PRISM_ERR_PROTOCOL; }
    rd r = { pl, pn, 12, 0 };
    uint8_t status = rd_u8(&r);
    uint64_t txn_id = rd_u64(&r);
    rd_u64(&r); /* commit_lsn */
    prism_status hs = read_trailer(c, &r, status);
    free(pl);
    if (hs) return hs;
    if (txn_id_out) *txn_id_out = txn_id;
    return PRISM_OK;
}
prism_status prism_begin(prism_client *c, int read_only, uint64_t *txn_id_out) {
    if (!c) return PRISM_ERR_USAGE;
    uint8_t body = (uint8_t)(read_only ? 1 : 0);
    return txn_xchg(c, MSG_BEGIN, &body, 1, txn_id_out);
}
prism_status prism_commit(prism_client *c, uint64_t idempotency_key) {
    if (!c) return PRISM_ERR_USAGE;
    buf b = {0};
    bw_u128(&b, idempotency_key);
    if (b.oom) { free(b.data); return PRISM_ERR_OOM; }
    prism_status st = txn_xchg(c, MSG_COMMIT, b.data, b.len, NULL);
    free(b.data);
    return st;
}
prism_status prism_abort(prism_client *c) {
    if (!c) return PRISM_ERR_USAGE;
    return txn_xchg(c, MSG_ABORT, NULL, 0, NULL);
}

/* ---- key/value ---------------------------------------------------------- */

prism_status prism_kv_get(prism_client *c, const char *ns, const void *key, size_t keylen,
                          uint8_t **out, size_t *outlen, int *found) {
    if (!c || !ns || !out || !outlen || !found) return PRISM_ERR_USAGE;
    *out = NULL; *outlen = 0; *found = 0;
    buf b = {0};
    bw_u8(&b, 1); /* get */
    bw_str16(&b, ns, strlen(ns));
    bw_bytes16(&b, key, keylen);
    if (b.oom) { free(b.data); return PRISM_ERR_OOM; }
    uint8_t *pl = NULL; size_t pn = 0; uint8_t mt = 0;
    prism_status st = xchg(c, MSG_KV_OP, b.data, b.len, &pl, &pn, &mt);
    free(b.data);
    if (st) return st;
    if (mt != MSG_KV_RESULT) { free(pl); return PRISM_ERR_PROTOCOL; }
    rd r = { pl, pn, 12, 0 };
    uint8_t status = rd_u8(&r);
    uint8_t op = rd_u8(&r);
    if (op != 1) { free(pl); return PRISM_ERR_PROTOCOL; }
    uint8_t fnd = rd_u8(&r);
    if (fnd) {
        uint32_t vlen = rd_u32(&r);
        const uint8_t *vp = rd_raw(&r, vlen);
        if (!vp) { free(pl); return PRISM_ERR_PROTOCOL; }
        uint8_t *copy = (uint8_t *)malloc(vlen ? vlen : 1);
        if (!copy) { free(pl); return PRISM_ERR_OOM; }
        if (vlen) memcpy(copy, vp, vlen);
        *out = copy;
        *outlen = vlen;
        *found = 1;
    }
    prism_status hs = read_trailer(c, &r, status);
    free(pl);
    if (hs) { free(*out); *out = NULL; *outlen = 0; *found = 0; return hs; }
    return PRISM_OK;
}

static prism_status kv_simple(prism_client *c, uint8_t op, const char *ns,
                              const void *key, size_t keylen,
                              const void *val, size_t vallen) {
    buf b = {0};
    bw_u8(&b, op);
    bw_str16(&b, ns, strlen(ns));
    bw_bytes16(&b, key, keylen);
    if (op == 2) bw_bytes32(&b, val, vallen);
    if (b.oom) { free(b.data); return PRISM_ERR_OOM; }
    uint8_t *pl = NULL; size_t pn = 0; uint8_t mt = 0;
    prism_status st = xchg(c, MSG_KV_OP, b.data, b.len, &pl, &pn, &mt);
    free(b.data);
    if (st) return st;
    if (mt != MSG_KV_RESULT) { free(pl); return PRISM_ERR_PROTOCOL; }
    rd r = { pl, pn, 12, 0 };
    uint8_t status = rd_u8(&r);
    rd_u8(&r); /* op */
    prism_status hs = read_trailer(c, &r, status);
    free(pl);
    return hs;
}
prism_status prism_kv_put(prism_client *c, const char *ns, const void *key, size_t keylen,
                          const void *val, size_t vallen) {
    if (!c || !ns) return PRISM_ERR_USAGE;
    return kv_simple(c, 2, ns, key, keylen, val, vallen);
}
prism_status prism_kv_delete(prism_client *c, const char *ns, const void *key, size_t keylen) {
    if (!c || !ns) return PRISM_ERR_USAGE;
    return kv_simple(c, 3, ns, key, keylen, NULL, 0);
}

/* ---- documents ---------------------------------------------------------- */

/* Decode a DocResult, returning affected + docs (caller frees docs array). */
static prism_status doc_xchg(prism_client *c, const uint8_t *body, size_t blen,
                             int64_t *affected_out, uint8_t first_id_out[12], int *got_id_out,
                             prism_doc ***docs_out, size_t *ndocs_out) {
    if (got_id_out) *got_id_out = 0;
    if (docs_out) { *docs_out = NULL; *ndocs_out = 0; }
    uint8_t *pl = NULL; size_t pn = 0; uint8_t mt = 0;
    prism_status st = xchg(c, MSG_DOC_OP, body, blen, &pl, &pn, &mt);
    if (st) return st;
    if (mt != MSG_DOC_RESULT) { free(pl); return PRISM_ERR_PROTOCOL; }
    rd r = { pl, pn, 12, 0 };
    uint8_t status = rd_u8(&r);
    int64_t affected = (int64_t)rd_u64(&r);
    uint32_t idc = rd_u32(&r);
    for (uint32_t i = 0; i < idc; i++) {
        const uint8_t *idp = rd_raw(&r, 12);
        if (i == 0 && idp && first_id_out) { memcpy(first_id_out, idp, 12); if (got_id_out) *got_id_out = 1; }
    }
    uint32_t dc = rd_u32(&r);
    prism_doc **docs = NULL;
    if (docs_out && dc) {
        docs = (prism_doc **)calloc(dc, sizeof(prism_doc *));
        if (!docs) { free(pl); return PRISM_ERR_OOM; }
    }
    for (uint32_t i = 0; i < dc; i++) {
        uint32_t dlen = rd_u32(&r);
        const uint8_t *dp = rd_raw(&r, dlen);
        if (!dp) { if (docs) prism_docs_free(docs, i); free(pl); return PRISM_ERR_PROTOCOL; }
        prism_doc *d = doc_decode(dp, dlen);
        if (!d) { if (docs) prism_docs_free(docs, i); free(pl); return PRISM_ERR_PROTOCOL; }
        if (docs) docs[i] = d; else prism_doc_free(d);
    }
    uint8_t more = rd_u8(&r);
    prism_status hs = read_trailer(c, &r, status);
    if (r.err || (more && !hs)) { if (docs) prism_docs_free(docs, dc); free(pl); return PRISM_ERR_PROTOCOL; }
    if (hs) { if (docs) prism_docs_free(docs, dc); free(pl); return hs; }
    free(pl);
    if (affected_out) *affected_out = affected;
    if (docs_out) { *docs_out = docs; *ndocs_out = dc; }
    return PRISM_OK;
}

void prism_docs_free(prism_doc **docs, size_t count) {
    if (!docs) return;
    for (size_t i = 0; i < count; i++) prism_doc_free(docs[i]);
    free(docs);
}

prism_status prism_doc_insert_one(prism_client *c, const char *coll, const prism_doc *doc,
                                  uint8_t oid_out[12]) {
    if (!c || !coll || !doc) return PRISM_ERR_USAGE;
    buf b = {0};
    bw_u8(&b, 1); /* insertOne */
    bw_str16(&b, coll, strlen(coll));
    buf docbuf = {0};
    if (doc_encode(doc, &docbuf)) { free(docbuf.data); free(b.data); return PRISM_ERR_OOM; }
    bw_bytes32(&b, docbuf.data, docbuf.len);
    free(docbuf.data);
    if (b.oom) { free(b.data); return PRISM_ERR_OOM; }
    int got = 0;
    prism_status st = doc_xchg(c, b.data, b.len, NULL, oid_out, &got, NULL, NULL);
    free(b.data);
    if (st) return st;
    if (oid_out && !got) memset(oid_out, 0, 12);
    return PRISM_OK;
}

static prism_status doc_find_impl(prism_client *c, uint8_t op, const char *coll,
                                  const prism_query *q, prism_doc ***docs_out, size_t *count_out) {
    if (!c || !coll || !docs_out || !count_out) return PRISM_ERR_USAGE;
    prism_query *tmp = NULL;
    if (!q) { tmp = prism_q_all(); if (!tmp) return PRISM_ERR_OOM; q = tmp; }
    buf b = {0};
    bw_u8(&b, op);
    bw_str16(&b, coll, strlen(coll));
    buf qb = {0};
    q_encode(&qb, q);
    if (qb.oom) { free(qb.data); free(b.data); prism_query_free(tmp); return PRISM_ERR_OOM; }
    bw_bytes32(&b, qb.data, qb.len);
    free(qb.data);
    bw_bytes32(&b, NULL, 0); /* options */
    if (b.oom) { free(b.data); prism_query_free(tmp); return PRISM_ERR_OOM; }
    prism_status st = doc_xchg(c, b.data, b.len, NULL, NULL, NULL, docs_out, count_out);
    free(b.data);
    prism_query_free(tmp);
    return st;
}
prism_status prism_doc_find(prism_client *c, const char *coll, const prism_query *q,
                            prism_doc ***docs_out, size_t *count_out) {
    return doc_find_impl(c, 3, coll, q, docs_out, count_out);
}
prism_status prism_doc_find_one(prism_client *c, const char *coll, const prism_query *q,
                                prism_doc ***docs_out, size_t *count_out) {
    return doc_find_impl(c, 4, coll, q, docs_out, count_out);
}

static prism_status doc_query_affected(prism_client *c, uint8_t op, const char *coll,
                                       const prism_query *q, const prism_update *u,
                                       int64_t *out) {
    if (!c || !coll) return PRISM_ERR_USAGE;
    prism_query *tmp = NULL;
    if (!q) { tmp = prism_q_all(); if (!tmp) return PRISM_ERR_OOM; q = tmp; }
    buf b = {0};
    bw_u8(&b, op);
    bw_str16(&b, coll, strlen(coll));
    buf qb = {0};
    q_encode(&qb, q);
    if (qb.oom) { free(qb.data); free(b.data); prism_query_free(tmp); return PRISM_ERR_OOM; }
    bw_bytes32(&b, qb.data, qb.len);
    free(qb.data);
    if (op == 5 || op == 6) { /* update: query, update, options */
        buf ub = {0};
        update_encode(&ub, u);
        if (ub.oom) { free(ub.data); free(b.data); prism_query_free(tmp); return PRISM_ERR_OOM; }
        bw_bytes32(&b, ub.data, ub.len);
        free(ub.data);
    }
    bw_bytes32(&b, NULL, 0); /* options */
    if (b.oom) { free(b.data); prism_query_free(tmp); return PRISM_ERR_OOM; }
    int64_t affected = 0;
    prism_status st = doc_xchg(c, b.data, b.len, &affected, NULL, NULL, NULL, NULL);
    free(b.data);
    prism_query_free(tmp);
    if (st) return st;
    if (out) *out = affected;
    return PRISM_OK;
}
prism_status prism_doc_count(prism_client *c, const char *coll, const prism_query *q, int64_t *out) {
    return doc_query_affected(c, 9, coll, q, NULL, out);
}
prism_status prism_doc_update_one(prism_client *c, const char *coll, const prism_query *q,
                                  const prism_update *u, int64_t *out) {
    return doc_query_affected(c, 5, coll, q, u, out);
}
prism_status prism_doc_update_many(prism_client *c, const char *coll, const prism_query *q,
                                   const prism_update *u, int64_t *out) {
    return doc_query_affected(c, 6, coll, q, u, out);
}
prism_status prism_doc_delete_one(prism_client *c, const char *coll, const prism_query *q, int64_t *out) {
    return doc_query_affected(c, 7, coll, q, NULL, out);
}
prism_status prism_doc_delete_many(prism_client *c, const char *coll, const prism_query *q, int64_t *out) {
    return doc_query_affected(c, 8, coll, q, NULL, out);
}
