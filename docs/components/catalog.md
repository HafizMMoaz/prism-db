# Component: Catalog

**Crate:** `prism-core`
**Status:** Accepted
**Last updated:** 2026-05-15

## Purpose

The catalog stores metadata about every user-visible object: tables, columns, indexes, collections, KV namespaces, users, roles, permissions. It is stored in the same engine as user data — the catalog is just a set of system tables, bootstrapped at database creation. This means catalog modifications are transactional: a `CREATE TABLE` commits like any other transaction; a crash leaves no half-created tables.

## System tables

```
_prism_tables           one row per relational table
_prism_columns          one row per column
_prism_indexes          one row per index (relational, document, or KV)
_prism_collections      one row per document collection
_prism_kv_namespaces    one row per KV namespace
_prism_users            one row per user
_prism_roles            one row per role
_prism_grants           one row per permission grant
_prism_sequences        one row per named sequence (for ID generation)
```

All system tables have names beginning with `_prism_`. The prefix is reserved; user code cannot create tables with this prefix.

System tables are stored as ordinary heap files with B+tree primary indexes. They participate in MVCC like everything else.

## Bootstrap

When a database is first created:

1. Disk manager creates the heap file with the database header on page 0.
2. The bootstrap routine, hard-coded in Rust:
   a. Allocates pages for each system table and its primary index.
   b. Initializes system tables with self-describing rows (the row describing `_prism_tables` is itself a row in `_prism_tables`).
   c. Records the resulting RIDs in a small bootstrap section of page 0 (the database header) so that on startup the catalog can be loaded without circular dependency.

Bootstrap is run inside a single transaction. If it fails midway, the database file is incomplete; the operator's only recourse is to start over.

## Loading at startup

At engine startup, after recovery completes:

1. Read database header. Get the bootstrap section: pointers to `_prism_tables`, `_prism_columns`, `_prism_indexes`.
2. Load the rows of `_prism_tables` into an in-memory map: `oid -> TableMetadata`.
3. Load `_prism_columns` into per-table column lists.
4. Load `_prism_indexes` into per-table index lists.
5. Repeat for documents, KV namespaces, users, roles, grants.

The in-memory catalog is a `RwLock<CatalogSnapshot>` updated atomically on DDL commits.

## Object IDs (OIDs)

Every object has a 64-bit OID, allocated from a single global sequence. OIDs are never reused. They are the stable handle for objects across renames; if a user renames table `users` to `customers`, OIDs do not change, and code that has cached `oid -> ...` does not break.

## Schema for major tables

### `_prism_tables`
| Column | Type | Description |
|---|---|---|
| oid | INT64 | primary key |
| name | TEXT | table name |
| owner | INT64 | user OID |
| created_at | TIMESTAMP | |
| primary_index | INT64 | OID of the primary index |

### `_prism_columns`
| Column | Type | Description |
|---|---|---|
| oid | INT64 | primary key |
| table_oid | INT64 | foreign key to `_prism_tables.oid` |
| position | INT32 | ordinal in the row |
| name | TEXT | column name |
| type | INT32 | type code (enum) |
| nullable | BOOL | |
| default_expr | TEXT | SQL expression, or NULL |
| unique | BOOL | |

### `_prism_indexes`
| Column | Type | Description |
|---|---|---|
| oid | INT64 | primary key |
| name | TEXT | index name |
| owner_oid | INT64 | OID of the table, collection, or namespace |
| owner_kind | INT8 | 1=table, 2=collection, 3=namespace |
| index_kind | INT8 | 1=btree, 2=hash |
| key_path | TEXT | for documents: dotted path; for SQL: column name |

### `_prism_collections`
| Column | Type | Description |
|---|---|---|
| oid | INT64 | primary key |
| name | TEXT | |
| owner | INT64 | |
| created_at | TIMESTAMP | |
| heap_root_page | INT64 | physical location |

### `_prism_kv_namespaces`
| Column | Type | Description |
|---|---|---|
| oid | INT64 | primary key |
| name | TEXT | |
| index_kind | INT8 | 1=btree, 2=hash |
| heap_root_page | INT64 | physical location |

### `_prism_users`
| Column | Type | Description |
|---|---|---|
| oid | INT64 | primary key |
| name | TEXT | |
| password_hash | BLOB | scrypt hash |
| created_at | TIMESTAMP | |

### `_prism_grants`
| Column | Type | Description |
|---|---|---|
| principal_oid | INT64 | user or role |
| object_oid | INT64 | table/collection/namespace |
| privileges | INT32 | bitmask: SELECT, INSERT, UPDATE, DELETE, CREATE_INDEX |

## DDL operations

Every DDL operation is a transaction that:
1. Acquires an exclusive schema lock on the affected object (if it exists) or on a special "create" intent.
2. Mutates the relevant system tables.
3. Updates the in-memory catalog snapshot via copy-on-write: build a new snapshot, atomically swap.
4. Commits.

If the transaction aborts, the system table rows are rolled back by MVCC and the in-memory snapshot is not swapped. Atomicity is automatic.

## Concurrency

- Reads of the catalog are wait-free: an `RwLock<Arc<CatalogSnapshot>>` is read-locked briefly to fetch the `Arc`, then released. Readers use the `Arc` indefinitely.
- DDL writes acquire the write lock on the `RwLock` for the swap. The swap itself is a pointer replacement; readers in flight against the old snapshot continue against the old snapshot until they release it.

DDL operations are rare. Optimizing for them is not interesting; correctness is.

## Permissions

Every operation checks permissions at the point of dispatch. The permission set per principal and object is small enough to cache.

| Operation | Permission required |
|---|---|
| SELECT on table | SELECT |
| INSERT/UPDATE/DELETE on table | corresponding |
| CREATE / DROP table | CREATE on database; or owner |
| CREATE INDEX | CREATE_INDEX on the table |
| CREATE USER, GRANT, etc. | OPERATOR role |

A built-in `OPERATOR` role exists; admin users are members. All catalog modifications require it.

## Configuration

The catalog has no user-facing configuration. The system table layout is fixed at compile time.

## Metrics

- `prism_catalog_table_count`
- `prism_catalog_collection_count`
- `prism_catalog_namespace_count`
- `prism_catalog_index_count`
- `prism_ddl_operations_total{op="create_table"|"drop_table"|...}`

## Testing

- Bootstrap: create a database, verify the system tables are populated correctly and consistent.
- DDL: create, alter, drop; verify rollback if the transaction aborts.
- Concurrency: concurrent DDL on different tables proceeds without serialization; concurrent DDL on the same table serializes correctly.
- Recovery: crash during DDL; verify recovery leaves no half-created objects.

## References

- ADR 0006 — single TxnManager covers DDL too.
- `components/sql-engine.md` — primary user of relational catalog.
- `components/document-engine.md` — uses collection metadata.
- `components/kv-engine.md` — uses namespace metadata.
- PostgreSQL `pg_class`, `pg_attribute`, `pg_index` are the inspiration.
