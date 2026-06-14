/*
 * A quick tour of the C SDK against a running prismd.
 *
 *   prismd run ./data 127.0.0.1:4444
 *   make example && ./quickstart
 *   (or set PRISM_HOST / PRISM_PORT / PRISM_USER / PRISM_PASSWORD)
 */
#include "prism.h"

#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static const char *env(const char *k, const char *fallback) {
    const char *v = getenv(k);
    return (v && v[0]) ? v : fallback;
}

static int die(prism_client *c, prism_status st, const char *what) {
    fprintf(stderr, "%s: %s", what, prism_status_str(st));
    const prism_error *e = prism_last_error(c);
    if (e) fprintf(stderr, " (code 0x%04x, sqlstate %s: %s)", e->code, e->sqlstate, e->message);
    fprintf(stderr, "\n");
    prism_client_free(c);
    return 1;
}

int main(void) {
    prism_options o = prism_options_default();
    o.host = env("PRISM_HOST", "127.0.0.1");
    o.port = atoi(env("PRISM_PORT", "4444"));
    o.username = env("PRISM_USER", "admin");
    o.password = env("PRISM_PASSWORD", "admin");

    prism_client *c = NULL;
    prism_status st = prism_connect(&o, &c);
    if (st) { fprintf(stderr, "connect: %s\n", prism_status_str(st)); return 1; }

    /* --- SQL --- */
    prism_result *r = NULL;
    if ((st = prism_sql(c, "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)", NULL, 0, &r)))
        return die(c, st, "create");
    prism_result_free(r);
    if ((st = prism_sql(c, "INSERT INTO users VALUES (1,'alice',30),(2,'bob',25),(3,'carol',41)", NULL, 0, &r)))
        return die(c, st, "insert");
    prism_result_free(r);

    if ((st = prism_sql(c, "SELECT id, name, age FROM users WHERE age >= 30 ORDER BY age", NULL, 0, &r)))
        return die(c, st, "select");
    for (size_t row = 0; row < prism_result_rows(r); row++) {
        prism_value name = prism_result_get(r, row, 1);
        prism_value age = prism_result_get(r, row, 2);
        printf("  %.*s (%" PRId64 ")\n", (int)name.as.str.len, name.as.str.ptr, age.as.i64);
    }
    prism_result_free(r);

    /* --- KV --- */
    if ((st = prism_kv_put(c, "sessions", "sid-1", 5, "payload", 7))) return die(c, st, "kv put");
    uint8_t *val = NULL; size_t vlen = 0; int found = 0;
    if ((st = prism_kv_get(c, "sessions", "sid-1", 5, &val, &vlen, &found))) return die(c, st, "kv get");
    if (found) { printf("kv get: %.*s\n", (int)vlen, (char *)val); prism_free(val); }

    /* --- Documents --- */
    prism_doc *d = prism_doc_new();
    prism_doc_set(d, "name", prism_str("carol"));
    prism_doc_set(d, "age", prism_i64(41));
    prism_doc_set(d, "city", prism_str("NYC"));
    uint8_t oid[12];
    if ((st = prism_doc_insert_one(c, "people", d, oid))) return die(c, st, "doc insert");
    prism_doc_free(d);
    char hex[25];
    prism_objectid_to_hex(oid, hex);
    printf("inserted _id: %s\n", hex);

    prism_query *subs[2] = { prism_q_eq("city", prism_str("NYC")), prism_q_gt("age", prism_i64(30)) };
    prism_query *q = prism_q_and(subs, 2);
    prism_doc **docs = NULL; size_t ndocs = 0;
    if ((st = prism_doc_find(c, "people", q, &docs, &ndocs))) return die(c, st, "doc find");
    printf("NYC adults: %lu\n", (unsigned long)ndocs);
    prism_docs_free(docs, ndocs);
    prism_query_free(q);

    /* --- Transaction (atomic across models) --- */
    if ((st = prism_begin(c, 0, NULL))) return die(c, st, "begin");
    if ((st = prism_sql(c, "INSERT INTO users VALUES (4,'dave',50)", NULL, 0, &r))) return die(c, st, "tx insert");
    prism_result_free(r);
    if ((st = prism_kv_put(c, "sessions", "sid-2", 5, "tx", 2))) return die(c, st, "tx kv");
    if ((st = prism_commit(c, 0))) return die(c, st, "commit");

    if ((st = prism_sql(c, "SELECT COUNT(*) FROM users", NULL, 0, &r))) return die(c, st, "count");
    printf("user count: %" PRId64 "\n", prism_result_get(r, 0, 0).as.i64);
    prism_result_free(r);

    prism_client_free(c);
    return 0;
}
