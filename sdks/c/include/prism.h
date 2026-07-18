/*
 * prism.h - a pure-C client for PrismDB over the binary wire protocol.
 *
 * Single-header public API; the implementation lives in prism.c. Speaks
 * docs/specs/wire-protocol.md directly over a TCP socket (Winsock on Windows,
 * BSD sockets elsewhere). No third-party dependencies.
 *
 * Conventions
 *   - Functions that talk to the server return a prism_status (PRISM_OK == 0).
 *     On PRISM_ERR_SERVER, call prism_last_error() for the structured trailer.
 *   - Output buffers documented as "caller frees" must be released with
 *     prism_free(); result/doc/query/update objects have dedicated *_free().
 *   - Input prism_value strings/bytes point to caller memory and are copied as
 *     needed during a call; decoded values are owned by their container.
 */
#ifndef PRISM_H
#define PRISM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---- status & errors ---------------------------------------------------- */

typedef enum {
    PRISM_OK = 0,
    PRISM_ERR_IO = 1,       /* socket / transport failure                    */
    PRISM_ERR_PROTOCOL = 2, /* framing / decode failure                      */
    PRISM_ERR_SERVER = 3,   /* server returned status != 0 (see last_error)  */
    PRISM_ERR_USAGE = 4,    /* invalid arguments / unsupported option        */
    PRISM_ERR_OOM = 5       /* allocation failure                            */
} prism_status;

typedef struct {
    uint32_t code;       /* wire error code (see docs/specs/wire-protocol.md) */
    uint32_t position;   /* character offset in source SQL, or 0             */
    char sqlstate[6];    /* 5-char SQLSTATE + NUL                            */
    char message[256];
    char detail[256];
} prism_error;

/* ---- value model -------------------------------------------------------- */

typedef enum {
    PRISM_TAG_NULL = 0x00,
    PRISM_TAG_BOOL = 0x01,
    PRISM_TAG_INT32 = 0x02,
    PRISM_TAG_INT64 = 0x03,
    PRISM_TAG_DOUBLE = 0x04,
    PRISM_TAG_STRING = 0x05,
    PRISM_TAG_BINARY = 0x06,
    PRISM_TAG_TIMESTAMP = 0x09,
    PRISM_TAG_OBJECTID = 0x0A
} prism_tag;

typedef struct {
    prism_tag tag;
    union {
        int b;          /* PRISM_TAG_BOOL                                    */
        int32_t i32;    /* PRISM_TAG_INT32                                   */
        int64_t i64;    /* PRISM_TAG_INT64 / PRISM_TAG_TIMESTAMP            */
        double f64;     /* PRISM_TAG_DOUBLE                                  */
        struct { const char *ptr; size_t len; } str;   /* PRISM_TAG_STRING  */
        struct { const uint8_t *ptr; size_t len; } bin; /* PRISM_TAG_BINARY */
        uint8_t oid[12];                                /* PRISM_TAG_OBJECTID*/
    } as;
} prism_value;

prism_value prism_null(void);
prism_value prism_bool(int b);
prism_value prism_i32(int32_t v);
prism_value prism_i64(int64_t v);
prism_value prism_f64(double v);
prism_value prism_str(const char *s);            /* NUL-terminated UTF-8     */
prism_value prism_strn(const char *s, size_t n);
prism_value prism_bytes(const uint8_t *p, size_t n);
prism_value prism_timestamp(int64_t micros);
prism_value prism_objectid(const uint8_t oid[12]);

/* 24-char lowercase hex of a 12-byte ObjectId. `hex` must hold >= 25 bytes. */
void prism_objectid_to_hex(const uint8_t oid[12], char hex[25]);
/* Parse 24-char hex into 12 bytes. Returns PRISM_OK or PRISM_ERR_USAGE. */
prism_status prism_objectid_from_hex(const char *hex, uint8_t oid[12]);

/* ---- client ------------------------------------------------------------- */

typedef struct prism_client prism_client;

typedef struct {
    const char *host;          /* default "127.0.0.1"                        */
    int port;                  /* default 4444                               */
    const char *username;      /* NULL = skip authentication                 */
    const char *password;      /* NULL treated as ""                         */
    const char *database;      /* NULL/"" = no connect-time database         */
    int use_tls;               /* reserved; not supported by the C core yet  */
    int connect_timeout_ms;    /* default 10000                              */
    const char *client_name;   /* default "prismdb-c"                        */
    const char *client_version;/* default "0.1.0"                            */
} prism_options;

/* Zero-initialise then fill; or use prism_options_default() for the defaults. */
prism_options prism_options_default(void);

prism_status prism_connect(const prism_options *opts, prism_client **out);
void prism_client_free(prism_client *c);

/* The structured error from the most recent PRISM_ERR_SERVER on this client. */
const prism_error *prism_last_error(const prism_client *c);

/* A short human-readable string for a status (for logging). */
const char *prism_status_str(prism_status s);

/* Round-trip a keep-alive ping. */
prism_status prism_ping(prism_client *c);

/* Free a buffer returned by the library (e.g. prism_kv_get). */
void prism_free(void *p);

/* ---- SQL ---------------------------------------------------------------- */

typedef struct prism_result prism_result;

/* Execute SQL. `params`/`nparams` may be NULL/0. On PRISM_OK, *out holds a
 * result set the caller frees with prism_result_free(). */
prism_status prism_sql(prism_client *c, const char *sql,
                       const prism_value *params, size_t nparams,
                       prism_result **out);

size_t prism_result_rows(const prism_result *r);
size_t prism_result_cols(const prism_result *r);
const char *prism_result_col_name(const prism_result *r, size_t col);
int64_t prism_result_affected(const prism_result *r);
/* Borrowed value, valid until prism_result_free(). NULL cells have tag NULL. */
prism_value prism_result_get(const prism_result *r, size_t row, size_t col);
void prism_result_free(prism_result *r);

/* ---- transactions ------------------------------------------------------- */

prism_status prism_begin(prism_client *c, int read_only, uint64_t *txn_id_out);
prism_status prism_commit(prism_client *c, uint64_t idempotency_key);
prism_status prism_abort(prism_client *c);

/* ---- key/value ---------------------------------------------------------- */

/* On PRISM_OK with *found==1, *out is a malloc'd buffer of *outlen bytes the
 * caller frees with prism_free(). When *found==0, *out is NULL. */
prism_status prism_kv_get(prism_client *c, const char *ns,
                          const void *key, size_t keylen,
                          uint8_t **out, size_t *outlen, int *found);
prism_status prism_kv_put(prism_client *c, const char *ns,
                          const void *key, size_t keylen,
                          const void *val, size_t vallen);
prism_status prism_kv_delete(prism_client *c, const char *ns,
                             const void *key, size_t keylen);

/* ---- documents ---------------------------------------------------------- */

typedef struct prism_doc prism_doc;

prism_doc *prism_doc_new(void);
void prism_doc_free(prism_doc *d);
/* Append/replace a field. The value's bytes are copied. */
prism_status prism_doc_set(prism_doc *d, const char *field, prism_value v);
size_t prism_doc_len(const prism_doc *d);
/* Borrowed name/value for field index i (0..len). */
const char *prism_doc_field_name(const prism_doc *d, size_t i);
prism_value prism_doc_at(const prism_doc *d, size_t i);
/* Borrowed value for a named field; sets *found. */
prism_value prism_doc_get(const prism_doc *d, const char *field, int *found);

typedef struct prism_query prism_query;

prism_query *prism_q_all(void);
prism_query *prism_q_eq(const char *field, prism_value v);
prism_query *prism_q_ne(const char *field, prism_value v);
prism_query *prism_q_gt(const char *field, prism_value v);
prism_query *prism_q_lt(const char *field, prism_value v);
prism_query *prism_q_gte(const char *field, prism_value v);
prism_query *prism_q_lte(const char *field, prism_value v);
prism_query *prism_q_in(const char *field, const prism_value *vals, size_t n);
prism_query *prism_q_nin(const char *field, const prism_value *vals, size_t n);
prism_query *prism_q_exists(const char *field, int present);
/* and/or take ownership of the sub-queries (freed with the parent). */
prism_query *prism_q_and(prism_query **subs, size_t n);
prism_query *prism_q_or(prism_query **subs, size_t n);
prism_query *prism_q_not(prism_query *sub);
void prism_query_free(prism_query *q);

typedef struct prism_update prism_update;

prism_update *prism_update_new(void);
void prism_update_free(prism_update *u);
prism_status prism_update_set(prism_update *u, const char *field, prism_value v);
prism_status prism_update_unset(prism_update *u, const char *field);
prism_status prism_update_inc(prism_update *u, const char *field, int64_t delta);

/* insert_one writes the assigned id (if non-NULL). */
prism_status prism_doc_insert_one(prism_client *c, const char *coll,
                                  const prism_doc *doc, uint8_t oid_out[12]);

/* find/find_one allocate an array of prism_doc* the caller frees with
 * prism_docs_free(). find_one yields 0 or 1 docs. A NULL query means "all". */
prism_status prism_doc_find(prism_client *c, const char *coll,
                            const prism_query *q,
                            prism_doc ***docs_out, size_t *count_out);
prism_status prism_doc_find_one(prism_client *c, const char *coll,
                                const prism_query *q,
                                prism_doc ***docs_out, size_t *count_out);
void prism_docs_free(prism_doc **docs, size_t count);

prism_status prism_doc_count(prism_client *c, const char *coll,
                             const prism_query *q, int64_t *out);
prism_status prism_doc_update_one(prism_client *c, const char *coll,
                                  const prism_query *q, const prism_update *u,
                                  int64_t *affected_out);
prism_status prism_doc_update_many(prism_client *c, const char *coll,
                                   const prism_query *q, const prism_update *u,
                                   int64_t *affected_out);
prism_status prism_doc_delete_one(prism_client *c, const char *coll,
                                  const prism_query *q, int64_t *affected_out);
prism_status prism_doc_delete_many(prism_client *c, const char *coll,
                                   const prism_query *q, int64_t *affected_out);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* PRISM_H */
