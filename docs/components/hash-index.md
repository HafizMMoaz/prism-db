# Component: Hash Index

**Crate:** `prism-index`
**Status:** Accepted
**Last updated:** 2026-05-15

## Purpose

The hash index supports point lookups (and only point lookups) by key. It is the primary index for KV namespaces, where range scans are opt-in and most workloads are point gets/puts. It can also be used by SQL when the planner can prove a query is point-only and a hash index is declared.

We use **extendible hashing** (Fagin et al. 1979), which gracefully handles growth without rehashing the entire table.

## Why extendible (vs. linear, vs. open-addressed, vs. cuckoo)

| Scheme | Pros | Cons |
|---|---|---|
| Linear hashing | Smooth growth, in-memory friendly | Awkward on-disk; modest variance per bucket |
| Extendible | On-disk friendly; constant-time lookup; gradual directory growth | Directory can be large for very large tables |
| Open-addressed | Cache-friendly | Resize-the-whole-table problem; bad on-disk |
| Cuckoo | Worst-case O(1) lookup | Complex insert path, especially in concurrent setting |

Extendible is the established on-disk hash design (used by Postgres's hash index, partially). It maps keys to buckets via a prefix of the hash; when a bucket overflows, only that bucket is split, and the directory is extended only when needed.

## Structure

```
Directory: array of bucket page IDs, size = 2^global_depth
Each bucket: a page with local_depth, entries [(hash_prefix, key, rid)], and an overflow chain
```

```
Directory (in-memory):
  global_depth = 8
  buckets[0..256]: PageId

Each bucket page:
  local_depth: u8
  entry_count: u16
  overflow_page_id: PageId
  entries: [(hash: u32, key_offset: u16, key_len: u16, rid: RecordId)]
```

Hash function: `xxhash3` (fast, decent distribution).

To look up key K:
1. h = hash(K).
2. bucket_idx = h >> (32 - global_depth)   // top global_depth bits
3. page = directory[bucket_idx].
4. Read bucket page; scan entries for matching key.
5. If not found and overflow_page_id != NIL: follow overflow chain.

To insert (K, RID):
1. h, page = same as lookup.
2. If page has space: append entry.
3. If page is full:
   - If local_depth == global_depth:
     - Double the directory: global_depth += 1, every directory slot becomes two slots pointing at the same bucket.
     - Now local_depth < global_depth for this bucket.
   - Split the bucket:
     - Allocate a new bucket page.
     - local_depth += 1 for both old and new bucket.
     - Redistribute entries: those whose hash's local_depth-th bit is 0 stay; those whose bit is 1 go to the new bucket.
     - Update the directory: slots pointing at the old bucket whose bit-pattern requires it now point at the new bucket.
   - Retry insert.

To delete: scan, remove from entries array. Buckets are not merged on delete in v1.

## Directory persistence

The directory is small relative to the data. For a bucket page of 8 KiB holding ~500 entries, a 100-million-entry index has ~200,000 buckets, requiring at most 2^18 = 262144 entries in the directory, at 8 bytes each = 2 MiB.

The directory is stored in special pages (allocated at index creation, expanded as needed). Updates to the directory are logged in the WAL: `DirectoryUpdate { bucket_idx, old_page, new_page }`.

## Concurrency

- Lookups acquire a shared latch on the bucket page, scan, release.
- Inserts acquire an exclusive latch on the bucket page. If a split is needed:
  - Acquire an exclusive latch on the directory (or a sharded part of it).
  - Perform the split atomically: write the new bucket page, update the directory, update the old bucket page.
  - Log a single WAL record covering the structural change for clean recovery.

Directory expansion (doubling) is rare and acquires a global exclusive directory latch. Briefly stalls concurrent inserts.

## Index entries and MVCC

Same as B+tree: index entries are not version-tagged. Lookups produce a RID; the executor fetches and applies visibility. Stale entries are filtered.

## Range queries

The hash index does not support range queries. A KV namespace declared with `index = hash` rejects range operations with `RangeNotSupported`.

If the user wants both point and range, they declare `index = btree` (ordered). For pure point workloads, hash is faster (constant lookup vs. O(log n) tree descent).

## Recovery

WAL records:
- `HashInsert { bucket_page, key, rid }`
- `HashDelete { bucket_page, key, rid }`
- `BucketSplit { old_bucket, new_bucket, split_bit }`
- `DirectoryExpand { new_global_depth }`
- `DirectoryUpdate { bucket_idx, page }`

Replay re-applies these against page contents. page_lsn filters apply.

## Configuration

```toml
[hash_index]
initial_global_depth = 4         # 16 buckets at creation
bucket_overflow_threshold = 0.9  # fraction of bucket capacity before split
```

## Metrics

- `prism_hash_lookups_total`
- `prism_hash_inserts_total`
- `prism_hash_splits_total`
- `prism_hash_directory_expansions_total`
- `prism_hash_overflow_chain_length` (histogram per lookup)

## Testing

- Unit: insert, lookup, delete, split, overflow.
- Property: random workloads with random hash collisions; verify all keys found.
- Concurrent: many threads inserting; no lost inserts or panics.
- Hash distribution: under realistic key distributions, buckets remain balanced (< 2x mean).

## References

- Fagin, Nievergelt, Pippenger, Strong: "Extendible Hashing — A Fast Access Method for Dynamic Files." ACM TODS 1979.
- PostgreSQL hash access method (limited use historically; we are doing better with the WAL integration).
- ADR 0005 — unified record format; values are RIDs.
- `components/kv-engine.md` — primary user.
