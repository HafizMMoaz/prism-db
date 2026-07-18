# Component: KV Engine

**Crate:** `prism-kv`
**Status:** Accepted
**Last updated:** 2026-05-15

## Purpose

The KV engine provides byte-string keys mapped to byte-string values, organized into namespaces. It is the simplest of the three access methods and ships first as a smoke test of the underlying record store.

## Concepts

- **Namespace:** a logical KV store, named like a path. Examples: `cache:sessions`, `tokens:api`. A namespace has an index type (hash or btree) chosen at creation time.
- **Key:** opaque bytes, 1 to 1024 bytes.
- **Value:** opaque bytes, 0 to (page_size - record_header - key_len - small_overhead) ≈ 8 KiB minus a few hundred bytes.

Larger values are post-v1.

## Public interface

```rust
pub struct KvNamespace { /* opaque */ }

impl KvNamespace {
    pub fn get(&self, txn: &TxnHandle, key: &[u8]) -> Result<Option<Vec<u8>>>;
    pub fn put(&self, txn: &TxnHandle, key: &[u8], value: &[u8]) -> Result<()>;
    pub fn delete(&self, txn: &TxnHandle, key: &[u8]) -> Result<bool>;
    pub fn range(&self, txn: &TxnHandle, start: &[u8], end: &[u8]) -> Result<RangeIterator>;
    pub fn scan(&self, txn: &TxnHandle, prefix: &[u8]) -> Result<ScanIterator>;
}
```

`range` and `scan` are only supported on btree-indexed namespaces. Hash-indexed namespaces return `RangeNotSupported` for these.

## Storage

A KV namespace is a heap of records:

```
RecordHeader { xmin, xmax, ... }
KvPayload { key_len: u16, key, value }
```

The namespace has one primary index over the keys. Index entries map `key → RID`.

`get(key)`:
1. Index lookup → RID.
2. Read record at RID via the record store (MVCC visibility).
3. Decode payload, return value bytes.

`put(key, value)`:
1. Index lookup. If key exists with a visible version:
   a. Acquire write lock on the RID.
   b. Update the record via `record_store.update`. Record store handles the version chain.
2. If key does not exist:
   a. Insert a new record via `record_store.insert`.
   b. Add `key → new_rid` to the index.

`delete(key)`:
1. Index lookup → RID.
2. Mark deleted via `record_store.delete`.
3. Remove the index entry. (Old entries pointing at the deleted version remain visible to readers with older snapshots - visibility filters them out.)

## Conditional operations

```rust
pub fn put_if_absent(&self, txn: &TxnHandle, key: &[u8], value: &[u8]) -> Result<bool>;
pub fn compare_and_set(&self, txn: &TxnHandle, key: &[u8], expected: &[u8], new: &[u8]) -> Result<bool>;
```

These compose ordinary get + put inside a transaction. They are convenience methods, not optimizations; an explicit transaction with explicit get/put has the same semantics.

## Range queries (btree namespaces only)

`range(start, end)` returns visible `(key, value)` pairs where `start <= key < end`, in ascending key order. Internally walks the B+tree's leaf-sibling chain.

`scan(prefix)` returns visible pairs where the key has the given byte prefix. Equivalent to `range(prefix, prefix_plus_one)` where `prefix_plus_one` is the lexicographically-next prefix.

Iterators are snapshot-consistent: every entry returned is visible to the iterator's snapshot.

## Concurrency

Records are records; MVCC and the lock manager apply. Concurrent `put` to the same key follows snapshot isolation; one wins, the other gets `SerializationFailure` if their snapshots are incompatible.

Concurrent `put` to different keys does not contend at the lock manager. Indexed under btree, they may contend on adjacent B+tree leaf pages; under hash, they may contend on the same bucket page. Page-level latches make these brief.

## TTL (post-v1)

Mongo and Redis both support TTL on keys. We do not in v1. Users can write a cleanup job in their application; the engine does not expire entries automatically. TTL is the most-requested KV feature for v1.1.

## Recovery

Records and index entries are WAL-logged. Recovery replays them. Nothing KV-specific.

## Configuration

```toml
[kv]
max_key_size = 1024
default_index_kind = "hash"
```

## Metrics

- `prism_kv_gets_total`
- `prism_kv_puts_total`
- `prism_kv_deletes_total`
- `prism_kv_get_hits_total`
- `prism_kv_get_misses_total`
- `prism_kv_range_scans_total`

## Testing

- Unit: each operation.
- Property: random workload; verify get returns the most recent put, deletes remove, ranges are ordered.
- Index parity: hash and btree namespaces produce identical results for point operations.
- Recovery: KV operations survive crash.

## References

- ADR 0005 - record format includes the KV payload format.
- ADR 0006 - cross-model transactions work because KV uses the same record store.
- `components/btree.md`, `components/hash-index.md` - the index choices.
