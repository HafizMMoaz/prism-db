# Component: B+tree Index

**Crate:** `prism-index`
**Status:** Accepted
**Last updated:** 2026-05-15

## Purpose

The B+tree is the primary ordered index structure for Prism. It supports point and range queries by key, with concurrent access via the Lehman-Yao protocol (a.k.a. B-link trees). It is the index used by SQL primary keys, SQL secondary indexes, document field indexes, and ordered KV namespaces.

## Variant: Lehman-Yao B-link tree

Classical B+trees suffer concurrency limitations: a writer descending the tree must hold latches on the path from root to leaf, blocking concurrent reads. The Lehman-Yao variant adds a right-sibling pointer to every node and a "high key" field; this allows writers to release latches as they descend, with readers handling the "what if my node was split since I latched it" case by following the right pointer.

The result is highly concurrent reads and writes with only leaf-level latching for most operations.

## Node format

Stored as a page in the heap file. The page header carries a node type discriminator.

### Internal node
```
┌──────────────────────────────────────┐
│ NodeHeader                            │
│   node_type: Internal                 │
│   level: u16                          │
│   key_count: u16                      │
│   right_sibling: PageId               │
│   high_key: variable-length bytes     │
├──────────────────────────────────────┤
│ Slot array (key_count + 1 entries):   │
│   [child_page_id, key_offset]         │
├──────────────────────────────────────┤
│ Key data (grows from end):            │
│   ... key bytes ...                   │
└──────────────────────────────────────┘
```

Internal nodes store routing keys. Key K at slot i means "anything < K goes to child i; anything >= K goes to child i+1." The last slot has no key (its child is "everything >= the last key").

### Leaf node
```
┌──────────────────────────────────────┐
│ NodeHeader                            │
│   node_type: Leaf                     │
│   level: 0                            │
│   entry_count: u16                    │
│   right_sibling: PageId               │
│   high_key: variable-length bytes     │
├──────────────────────────────────────┤
│ Slot array (entry_count entries):     │
│   [key_offset, value_offset]          │
├──────────────────────────────────────┤
│ Key + value data:                     │
│   each entry: [key_len, key, rid (8B)]│
└──────────────────────────────────────┘
```

Leaf values are always `RecordId` (8 bytes). The index does not store the indexed value itself; that lives in the record store.

## Key encoding

Keys are byte strings. Comparison is byte-wise. Numeric and string types must be serialized into byte-comparable form:

- Integers: big-endian, sign-flip the high bit for two's complement.
- Floats: IEEE 754, then sign-flip the high bit if positive, complement all bits if negative.
- Strings: UTF-8 bytes (sort order is byte-wise, not collation-aware in v1).
- Booleans: 0x00 for false, 0x01 for true.
- Composite (for future multi-column indexes): concatenation of fixed-width-encoded components.

The encoding is the responsibility of the caller (SQL planner, document indexer); the B+tree sees only bytes.

## Operations

### Point lookup

```
fn search(key: &[u8]) -> Option<RecordId>:
    page = root_page_id
    loop:
        node = buffer_pool.fetch_read(page)
        if node.is_leaf():
            // Lehman-Yao: maybe we landed on the wrong leaf due to a split.
            while key > node.high_key && node.right_sibling != NIL:
                node = buffer_pool.fetch_read(node.right_sibling)
            return node.find(key)
        else:
            page = node.route(key)
```

No latch is held across the loop (each `fetch_read` releases when the local binding leaves scope). The right-sibling chase is bounded: typically zero hops, occasionally one if a split happened between when we read the parent's pointer and when we got here.

### Range scan

```
fn range(start: &[u8], end: &[u8]) -> Iterator<(Key, RecordId)>:
    leaf = find_leaf_containing(start)
    loop:
        for entry in leaf.entries() where start <= entry.key < end:
            yield (entry.key, entry.rid)
        if leaf.high_key >= end or leaf.right_sibling == NIL:
            return
        leaf = buffer_pool.fetch_read(leaf.right_sibling)
```

Range scans use the right-sibling chain to traverse leaves in order without re-descending the tree.

### Insert

```
fn insert(key: &[u8], rid: RecordId):
    // Descend to the target leaf with write latches on the path.
    path = descend_with_write_latches(key)
    leaf = path.last()
    
    if leaf.has_space_for(key, rid):
        leaf.insert(key, rid)
        wal.append(IndexInsert { page: leaf.id, key, rid })
        leaf.page_lsn = lsn
        release_path()
        return
    
    // Need to split.
    split(path, key, rid)
```

The split walks back up the path: split the leaf, propagate the new pivot key to the parent, split the parent if needed, recurse to the root. If the root splits, create a new root.

Lehman-Yao detail: when splitting a leaf, the new right sibling is allocated and linked **before** the parent is updated. Readers traversing during the split may land on the old leaf; the right-sibling chase finds them the new one.

### Delete

```
fn delete(key: &[u8], rid: RecordId):
    leaf = find_leaf_containing(key)
    leaf.write_latch()
    leaf.remove(key, rid)
    wal.append(IndexDelete { page: leaf.id, key, rid })
    leaf.page_lsn = lsn
    leaf.write_unlatch()
```

Deletion does not merge underfull leaves in v1. This means leaves can become sparse and pages can be underutilized; it does not affect correctness. Page reclamation and merging are a v2 enhancement.

## Concurrency

- **Reads:** acquire shared latch on each node along the path; release as we descend. Lehman-Yao right-chase handles concurrent splits.
- **Writes:** acquire exclusive latches on the entire path from root to leaf. Release as we ascend after a split completes.
- **Deletions:** exclusive latch on the affected leaf only.

A write-write conflict at the same key value is impossible: write locks are taken on the RID by the lock manager (a higher layer); index inserts of the same `(key, rid)` pair don't conflict because the rid differs (every insert produces a new rid via MVCC; the old key/rid is logically obsolete but physically still present).

## Index entries and MVCC

Index entries are not version-tagged. After fetching `(key, rid)` from the index, the executor calls into the record store to fetch the visible version at `rid`, which may walk a version chain. Stale entries (pointing at versions invisible to the reader) are filtered there.

Consequence: indexes can contain entries that point at no-longer-current versions. Over time, indexes grow with dead entries. Vacuum (post-v1) prunes them.

## Recovery

Every index modification produces a WAL record. Replay re-applies the modification:

- `IndexInsert { page, key, rid }`: insert into the page if not already there (idempotent).
- `IndexDelete { page, key, rid }`: remove from the page if present.
- `PageSplit { left_page, right_page, pivot_key }`: ensure the split structure.

The page_lsn machinery filters out replays of operations already on disk.

## Configuration

The B+tree has no user-facing configuration; the page size (8 KiB) is fixed, and the node format is determined by that.

## Metrics

- `prism_btree_searches_total`
- `prism_btree_range_scans_total`
- `prism_btree_inserts_total`
- `prism_btree_splits_total`
- `prism_btree_depth` (gauge per index)
- `prism_btree_search_latency_seconds`

## Testing

- Unit: every operation.
- Property: random insert/delete sequences; verify the tree's invariants (sorted, balanced, all leaves at the same level, every key reachable).
- Concurrent stress: many threads inserting and searching same keyspace; verify all inserts are visible, no lost inserts, no panics.
- Large-scale: 100M inserts on a single index; verify performance characteristics.

## References

- Lehman and Yao: "Efficient Locking for Concurrent Operations on B-Trees." ACM TODS 1981.
- PostgreSQL's nbtree (Postgres uses Lehman-Yao with extensions).
- ADR 0010 - index scan operators are part of the Volcano executor.
- `components/recovery.md` - how index pages recover.
