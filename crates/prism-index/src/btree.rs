//! A unique-key, ordered B+tree over the buffer pool.
//!
//! Maps byte-string keys (compared byte-wise) to `RecordId`, supporting point
//! lookup, upsert, delete, and ordered range scan. Nodes are pages: the 32-byte
//! Prism page header (`page_lsn`, checksum, type) followed by a packed node
//! body; leaves are linked by a right-sibling pointer for range scans. See
//! `docs/components/btree.md`.
//!
//! This is the single-threaded core (a coarse tree-wide lock serializes
//! operations). Lehman-Yao concurrent latching + high-key right-chase,
//! WAL-logging for recovery, and non-unique (duplicate-key) entries are deferred
//! to later increments. The on-disk body encoding is a pragmatic v1 form, to be
//! aligned with the exact slot layout in `docs/specs/page-format.md` when the
//! in-place/WAL increment lands.

use std::sync::{Arc, Mutex};

use prism_buffer::{BufferPool, PageWriteGuard};
use prism_core::RecordId;
use prism_storage::{PAGE_SIZE, PageId, PageType, checksum};
use prism_wal::record::RecordPayload;
use prism_wal::{LogRecord, Wal};

use crate::error::{IndexError, Result};

/// Offset where the node body begins (after the page header).
const BODY_START: usize = 32;
/// Maximum node body size, in bytes.
const MAX_BODY: usize = PAGE_SIZE - BODY_START;
/// NIL sentinel for a right-sibling pointer (page 0 is the db header).
const NIL: u64 = 0;

/// A leaf node: sorted `(key, rid)` entries, linked to its right sibling.
#[derive(Clone, Debug)]
struct Leaf {
    right_sibling: Option<PageId>,
    entries: Vec<(Vec<u8>, RecordId)>,
}

/// An internal node: `keys.len() + 1` child pointers; `keys[i]` separates
/// `children[i]` (< key) from `children[i+1]` (>= key).
#[derive(Clone, Debug)]
struct Internal {
    level: u16,
    right_sibling: Option<PageId>,
    keys: Vec<Vec<u8>>,
    children: Vec<PageId>,
}

#[derive(Clone, Debug)]
enum Node {
    Leaf(Leaf),
    Internal(Internal),
}

impl Internal {
    /// The child a search for `key` descends into.
    fn route(&self, key: &[u8]) -> PageId {
        self.children[self.keys.partition_point(|k| k.as_slice() <= key)]
    }

    /// Insert a separator `pivot` and the `new_right` child it introduces.
    fn insert_child(&mut self, pivot: Vec<u8>, new_right: PageId) {
        let pos = self
            .keys
            .partition_point(|k| k.as_slice() < pivot.as_slice());
        self.keys.insert(pos, pivot);
        self.children.insert(pos + 1, new_right);
    }
}

/// The ordered index.
///
/// The root lives at a **fixed page** for its whole lifetime: a root split moves
/// the old root's contents to a new page and rewrites the fixed page as the new
/// parent, so [`BTree::root_page`] never changes. That lets a catalog persist the
/// root once. Every node write is WAL-logged as a full-page image (redo-only),
/// so the tree's pages are crash-durable and rebuilt by recovery - no rescan.
pub struct BTree {
    buffer: Arc<BufferPool>,
    wal: Arc<Wal>,
    root: PageId,
    /// Serializes operations (the single-threaded core; Lehman-Yao latching is a
    /// later increment).
    lock: Mutex<()>,
    /// Max keys/entries per node before splitting (besides the byte-size bound).
    order: usize,
}

impl BTree {
    /// Create an empty B+tree (a single empty leaf root), splitting only when a
    /// node fills a page.
    pub fn create(buffer: Arc<BufferPool>, wal: Arc<Wal>) -> Result<Self> {
        Self::with_order(buffer, wal, usize::MAX)
    }

    /// Create an empty B+tree that also splits once a node exceeds `order`
    /// entries/keys (used by tests to force splits and multi-level trees).
    pub fn with_order(buffer: Arc<BufferPool>, wal: Arc<Wal>, order: usize) -> Result<Self> {
        // Temporary tree to allocate the root through the normal logged path.
        let mut tree = Self {
            buffer,
            wal,
            root: PageId(0),
            lock: Mutex::new(()),
            order: order.max(2),
        };
        let root = tree.alloc_node(&Node::Leaf(Leaf {
            right_sibling: None,
            entries: Vec::new(),
        }))?;
        tree.root = root;
        Ok(tree)
    }

    /// Reopen an existing tree rooted at `root_page` (its fixed root).
    pub fn open(buffer: Arc<BufferPool>, wal: Arc<Wal>, root_page: PageId, order: usize) -> Self {
        Self {
            buffer,
            wal,
            root: root_page,
            lock: Mutex::new(()),
            order: order.max(2),
        }
    }

    /// The (fixed) root page, for persistence by the catalog.
    pub fn root_page(&self) -> PageId {
        self.root
    }

    /// Look up `key`, returning its `RecordId` if present.
    pub fn search(&self, key: &[u8]) -> Result<Option<RecordId>> {
        let _guard = self.lock.lock().expect("btree lock poisoned");
        let mut page = self.root;
        loop {
            match self.read_node(page)? {
                Node::Internal(n) => page = n.route(key),
                Node::Leaf(leaf) => {
                    let pos = leaf.entries.partition_point(|(k, _)| k.as_slice() < key);
                    return Ok(match leaf.entries.get(pos) {
                        Some((k, rid)) if k.as_slice() == key => Some(*rid),
                        _ => None,
                    });
                }
            }
        }
    }

    /// Insert or replace the entry for `key`.
    pub fn insert(&self, key: &[u8], rid: RecordId) -> Result<()> {
        let _guard = self.lock.lock().expect("btree lock poisoned");
        let root = self.root;
        if let Some((pivot, new_right)) = self.insert_into(root, key, rid)? {
            // The root split. To keep the root at its fixed page, move the old
            // root's (now left-half) contents to a fresh page and rewrite the
            // fixed root page as the new internal parent.
            let left_node = self.read_node(root)?;
            let level = match &left_node {
                Node::Internal(n) => n.level + 1,
                Node::Leaf(_) => 1,
            };
            let left_page = self.alloc_node(&left_node)?;
            self.write_node(
                root,
                &Node::Internal(Internal {
                    level,
                    right_sibling: None,
                    keys: vec![pivot],
                    children: vec![left_page, new_right],
                }),
            )?;
        }
        Ok(())
    }

    /// Delete `key`. Returns whether an entry was removed. (No leaf merging in
    /// v1; nodes may become sparse but stay correct.)
    pub fn delete(&self, key: &[u8]) -> Result<bool> {
        let _guard = self.lock.lock().expect("btree lock poisoned");
        let mut page = self.root;
        loop {
            match self.read_node(page)? {
                Node::Internal(n) => page = n.route(key),
                Node::Leaf(mut leaf) => {
                    let pos = leaf.entries.partition_point(|(k, _)| k.as_slice() < key);
                    if leaf
                        .entries
                        .get(pos)
                        .is_some_and(|(k, _)| k.as_slice() == key)
                    {
                        leaf.entries.remove(pos);
                        self.write_node(page, &Node::Leaf(leaf))?;
                        return Ok(true);
                    }
                    return Ok(false);
                }
            }
        }
    }

    /// All `(key, rid)` with `start <= key < end`, in ascending key order.
    pub fn range(&self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, RecordId)>> {
        let _guard = self.lock.lock().expect("btree lock poisoned");
        let mut page = self.root;
        while let Node::Internal(n) = self.read_node(page)? {
            page = n.route(start);
        }
        let mut out = Vec::new();
        let mut cursor = Some(page);
        while let Some(p) = cursor {
            let Node::Leaf(leaf) = self.read_node(p)? else {
                return Err(IndexError::Corrupt("expected leaf in sibling chain".into()));
            };
            for (k, rid) in &leaf.entries {
                if k.as_slice() >= end {
                    return Ok(out);
                }
                if k.as_slice() >= start {
                    out.push((k.clone(), *rid));
                }
            }
            cursor = leaf.right_sibling;
        }
        Ok(out)
    }

    // ── Insert recursion ─────────────────────────────────────────────────

    /// Insert into the subtree at `page`. Returns `Some((pivot, new_right))` if
    /// this node split and the parent must absorb a new separator/child.
    fn insert_into(
        &self,
        page: PageId,
        key: &[u8],
        rid: RecordId,
    ) -> Result<Option<(Vec<u8>, PageId)>> {
        match self.read_node(page)? {
            Node::Leaf(mut leaf) => {
                upsert(&mut leaf.entries, key, rid);
                if leaf.entries.len() <= self.order && leaf_size(&leaf) <= MAX_BODY {
                    self.write_node(page, &Node::Leaf(leaf))?;
                    return Ok(None);
                }
                // Split: left keeps [..mid], right takes [mid..].
                let mid = leaf.entries.len() / 2;
                let right_entries = leaf.entries.split_off(mid);
                let pivot = right_entries[0].0.clone();
                let right_page = self.alloc_node(&Node::Leaf(Leaf {
                    right_sibling: leaf.right_sibling,
                    entries: right_entries,
                }))?;
                leaf.right_sibling = Some(right_page);
                self.write_node(page, &Node::Leaf(leaf))?;
                Ok(Some((pivot, right_page)))
            }
            Node::Internal(mut inode) => {
                let child = inode.route(key);
                let Some((pivot, new_right)) = self.insert_into(child, key, rid)? else {
                    return Ok(None);
                };
                inode.insert_child(pivot, new_right);
                if inode.keys.len() <= self.order && internal_size(&inode) <= MAX_BODY {
                    self.write_node(page, &Node::Internal(inode))?;
                    return Ok(None);
                }
                // Split: median key is promoted (removed from both halves).
                let mid = inode.keys.len() / 2;
                let right_keys = inode.keys.split_off(mid + 1);
                let promoted = inode.keys.pop().expect("median key");
                let right_children = inode.children.split_off(mid + 1);
                let right_page = self.alloc_node(&Node::Internal(Internal {
                    level: inode.level,
                    right_sibling: inode.right_sibling,
                    keys: right_keys,
                    children: right_children,
                }))?;
                inode.right_sibling = Some(right_page);
                self.write_node(page, &Node::Internal(inode))?;
                Ok(Some((promoted, right_page)))
            }
        }
    }

    // ── Page I/O ─────────────────────────────────────────────────────────

    fn read_node(&self, page: PageId) -> Result<Node> {
        let guard = self.buffer.fetch_read(page)?;
        decode_node(&guard)
    }

    fn write_node(&self, page: PageId, node: &Node) -> Result<()> {
        let mut guard = self.buffer.fetch_write(page)?;
        put_node(&mut guard, node)?;
        self.log_page(&mut guard, page)
    }

    fn alloc_node(&self, node: &Node) -> Result<PageId> {
        let mut guard = self.buffer.new_page()?;
        let page = guard.page_id();
        put_node(&mut guard, node)?;
        self.log_page(&mut guard, page)?;
        Ok(page)
    }

    /// WAL-log the (already-written) page as a full-page image and stamp its
    /// `page_lsn` to that record, so the buffer pool won't flush it before the
    /// log is durable and recovery can rebuild it (redo-only). The checksum
    /// covers `[16..]`, so stamping the LSN at `[0..8]` leaves it valid.
    fn log_page(&self, guard: &mut PageWriteGuard<'_>, page: PageId) -> Result<()> {
        let image = guard[..].to_vec();
        let lsn = self
            .wal
            .append(LogRecord::system(RecordPayload::FullPageImage {
                page_id: page,
                image,
            }))?;
        guard.set_page_lsn(lsn);
        Ok(())
    }

    /// Collect every `(key, rid)` in order by walking the leftmost leaf then the
    /// right-sibling chain. Test helper / consistency check.
    #[cfg(test)]
    fn collect_all(&self) -> Result<Vec<(Vec<u8>, RecordId)>> {
        let _guard = self.lock.lock().expect("btree lock poisoned");
        let mut page = self.root;
        while let Node::Internal(n) = self.read_node(page)? {
            page = n.children[0];
        }
        let mut out = Vec::new();
        let mut cursor = Some(page);
        while let Some(p) = cursor {
            let Node::Leaf(leaf) = self.read_node(p)? else {
                return Err(IndexError::Corrupt("expected leaf".into()));
            };
            out.extend(leaf.entries);
            cursor = leaf.right_sibling;
        }
        Ok(out)
    }
}

/// Insert-or-replace `(key, rid)` into a sorted unique entry list.
fn upsert(entries: &mut Vec<(Vec<u8>, RecordId)>, key: &[u8], rid: RecordId) {
    let pos = entries.partition_point(|(k, _)| k.as_slice() < key);
    if entries.get(pos).is_some_and(|(k, _)| k.as_slice() == key) {
        entries[pos].1 = rid;
    } else {
        entries.insert(pos, (key.to_vec(), rid));
    }
}

fn leaf_size(leaf: &Leaf) -> usize {
    8 + 4
        + leaf
            .entries
            .iter()
            .map(|(k, _)| 4 + k.len() + 10)
            .sum::<usize>()
}

fn internal_size(n: &Internal) -> usize {
    2 + 8 + 4 + n.children.len() * 8 + n.keys.iter().map(|k| 4 + k.len()).sum::<usize>()
}

// ── Encoding ─────────────────────────────────────────────────────────────────

fn put_node(guard: &mut PageWriteGuard<'_>, node: &Node) -> Result<()> {
    let (page_type, body) = encode_body(node)?;
    if body.len() > MAX_BODY {
        return Err(IndexError::EntryTooLarge);
    }
    guard[0..8].fill(0); // page_lsn (no WAL logging for the index yet)
    guard[10..PAGE_SIZE].fill(0); // clear type byte, reserved header, and body
    guard[10] = page_type as u8;
    guard[BODY_START..BODY_START + body.len()].copy_from_slice(&body);
    let bytes: &[u8; PAGE_SIZE] = guard;
    let crc = checksum::page_checksum(bytes);
    guard[8..10].copy_from_slice(&crc.to_le_bytes());
    Ok(())
}

fn encode_body(node: &Node) -> Result<(PageType, Vec<u8>)> {
    let mut out = Vec::new();
    match node {
        Node::Leaf(leaf) => {
            put_u64(&mut out, leaf.right_sibling.map_or(NIL, |p| p.as_u64()));
            put_u32(&mut out, leaf.entries.len() as u32);
            for (key, rid) in &leaf.entries {
                put_u32(&mut out, key.len() as u32);
                out.extend_from_slice(key);
                put_u64(&mut out, rid.page.as_u64());
                put_u16(&mut out, rid.slot);
            }
            Ok((PageType::BTreeLeaf, out))
        }
        Node::Internal(n) => {
            put_u16(&mut out, n.level);
            put_u64(&mut out, n.right_sibling.map_or(NIL, |p| p.as_u64()));
            put_u32(&mut out, n.keys.len() as u32);
            for &child in &n.children {
                put_u64(&mut out, child.as_u64());
            }
            for key in &n.keys {
                put_u32(&mut out, key.len() as u32);
                out.extend_from_slice(key);
            }
            Ok((PageType::BTreeInternal, out))
        }
    }
}

fn decode_node(page: &[u8; PAGE_SIZE]) -> Result<Node> {
    let body = &page[BODY_START..];
    match PageType::from_u8(page[10]) {
        Some(PageType::BTreeLeaf) => {
            let mut r = Reader::new(body);
            let right_sibling = nil_to_opt(r.u64()?);
            let count = r.u32()? as usize;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                let key = r.bytes_u32()?;
                let page_id = PageId(r.u64()?);
                let slot = r.u16()?;
                entries.push((key, RecordId::new(page_id, slot)));
            }
            Ok(Node::Leaf(Leaf {
                right_sibling,
                entries,
            }))
        }
        Some(PageType::BTreeInternal) => {
            let mut r = Reader::new(body);
            let level = r.u16()?;
            let right_sibling = nil_to_opt(r.u64()?);
            let key_count = r.u32()? as usize;
            let mut children = Vec::with_capacity(key_count + 1);
            for _ in 0..key_count + 1 {
                children.push(PageId(r.u64()?));
            }
            let mut keys = Vec::with_capacity(key_count);
            for _ in 0..key_count {
                keys.push(r.bytes_u32()?);
            }
            Ok(Node::Internal(Internal {
                level,
                right_sibling,
                keys,
                children,
            }))
        }
        _ => Err(IndexError::Corrupt(format!(
            "unexpected page type byte {}",
            page[10]
        ))),
    }
}

fn nil_to_opt(v: u64) -> Option<PageId> {
    if v == NIL { None } else { Some(PageId(v)) }
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .p
            .checked_add(n)
            .filter(|&e| e <= self.b.len())
            .ok_or_else(|| IndexError::Corrupt("index node truncated".into()))?;
        let s = &self.b[self.p..end];
        self.p = end;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Result<u64> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(u64::from_le_bytes(a))
    }
    fn bytes_u32(&mut self) -> Result<Vec<u8>> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_buffer::{BufferPool, Config as BufConfig};
    use prism_storage::DiskManager;
    use prism_testkit::TempDir;
    use prism_wal::{Config as WalConfig, SyncMode, Wal};
    use std::collections::BTreeMap;

    fn rid(n: u64) -> RecordId {
        RecordId::new(PageId(n), (n % 7) as u16)
    }

    fn buffer() -> (TempDir, Arc<BufferPool>, Arc<Wal>) {
        let tmp = TempDir::new("btree").unwrap();
        let disk = Arc::new(DiskManager::open(&tmp.path().join("heap.db"), true).unwrap());
        let wal = Arc::new(
            Wal::open(
                &tmp.path().join("wal"),
                WalConfig {
                    segment_size: 256 * 1024,
                    sync_mode: SyncMode::None,
                },
            )
            .unwrap(),
        );
        let bp =
            Arc::new(BufferPool::new(disk, wal.clone(), BufConfig { frame_count: 64 }).unwrap());
        (tmp, bp, wal)
    }

    #[test]
    fn insert_search_delete() {
        let (_t, bp, wal) = buffer();
        let tree = BTree::with_order(bp, wal, 4).unwrap();
        tree.insert(b"b", rid(2)).unwrap();
        tree.insert(b"a", rid(1)).unwrap();
        tree.insert(b"c", rid(3)).unwrap();
        assert_eq!(tree.search(b"a").unwrap(), Some(rid(1)));
        assert_eq!(tree.search(b"b").unwrap(), Some(rid(2)));
        assert_eq!(tree.search(b"z").unwrap(), None);

        // Upsert replaces.
        tree.insert(b"b", rid(20)).unwrap();
        assert_eq!(tree.search(b"b").unwrap(), Some(rid(20)));

        assert!(tree.delete(b"b").unwrap());
        assert_eq!(tree.search(b"b").unwrap(), None);
        assert!(!tree.delete(b"b").unwrap());
    }

    #[test]
    fn grows_multiple_levels_and_keeps_keys_sorted() {
        let (_t, bp, wal) = buffer();
        let tree = BTree::with_order(bp, wal, 4).unwrap(); // small order forces splits
        for i in 0..200u32 {
            tree.insert(&i.to_be_bytes(), rid(i as u64)).unwrap();
        }
        // All present.
        for i in 0..200u32 {
            assert_eq!(tree.search(&i.to_be_bytes()).unwrap(), Some(rid(i as u64)));
        }
        // The leaf chain yields keys in sorted order.
        let all = tree.collect_all().unwrap();
        assert_eq!(all.len(), 200);
        assert!(all.windows(2).all(|w| w[0].0 < w[1].0));
        // The root is now internal (multi-level).
        assert!(matches!(
            tree.read_node(tree.root_page()).unwrap(),
            Node::Internal(_)
        ));
    }

    #[test]
    fn range_scan_is_ordered_and_bounded() {
        let (_t, bp, wal) = buffer();
        let tree = BTree::with_order(bp, wal, 4).unwrap();
        for i in 0..50u32 {
            tree.insert(&i.to_be_bytes(), rid(i as u64)).unwrap();
        }
        let got = tree
            .range(&10u32.to_be_bytes(), &20u32.to_be_bytes())
            .unwrap();
        let keys: Vec<u32> = got
            .iter()
            .map(|(k, _)| u32::from_be_bytes(k[..4].try_into().unwrap()))
            .collect();
        assert_eq!(keys, (10..20).collect::<Vec<_>>());
    }

    proptest::proptest! {
        #[test]
        fn matches_btreemap_oracle(ops in proptest::collection::vec(
            (proptest::bool::ANY, 0u16..64, 0u64..1000),
            0..400,
        )) {
            let (_t, bp, wal) = buffer();
            let tree = BTree::with_order(bp, wal, 4).unwrap();
            let mut model: BTreeMap<Vec<u8>, RecordId> = BTreeMap::new();

            for (is_insert, k, rv) in ops {
                let key = k.to_be_bytes().to_vec();
                if is_insert {
                    let r = rid(rv);
                    tree.insert(&key, r).unwrap();
                    model.insert(key.clone(), r);
                } else {
                    let removed = tree.delete(&key).unwrap();
                    proptest::prop_assert_eq!(removed, model.remove(&key).is_some());
                }
                proptest::prop_assert_eq!(tree.search(&key).unwrap(), model.get(&key).copied());
            }

            // Full-state agreement, in order.
            let got = tree.collect_all().unwrap();
            let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), *v)).collect();
            proptest::prop_assert_eq!(got, expected);

            // A bounded range agrees with the model.
            let (lo, hi) = (10u16.to_be_bytes().to_vec(), 40u16.to_be_bytes().to_vec());
            let got_range = tree.range(&lo, &hi).unwrap();
            let exp_range: Vec<_> = model
                .range(lo.clone()..hi.clone())
                .map(|(k, v)| (k.clone(), *v))
                .collect();
            proptest::prop_assert_eq!(got_range, exp_range);
        }
    }

    #[test]
    fn tree_survives_restart_via_wal() {
        use prism_core::recover;
        use prism_storage::DiskManager as Disk;

        let tmp = TempDir::new("btree-restart").unwrap();
        let heap_path = tmp.path().join("heap.db");
        let wal_path = tmp.path().join("wal");
        let wal_cfg = WalConfig {
            segment_size: 1 << 20,
            sync_mode: SyncMode::None,
        };

        // Session 1: build a multi-level tree, make its WAL durable, then crash.
        let root = {
            let disk = Arc::new(Disk::open(&heap_path, true).unwrap());
            let wal = Arc::new(Wal::open(&wal_path, wal_cfg).unwrap());
            let bp = Arc::new(
                BufferPool::new(disk, wal.clone(), BufConfig { frame_count: 16 }).unwrap(),
            );
            let tree = BTree::with_order(bp, wal.clone(), 4).unwrap();
            for i in 0..200u32 {
                tree.insert(&i.to_be_bytes(), rid(i as u64)).unwrap();
            }
            let root = tree.root_page();
            // No txn drives a flush here, so flush the index's WAL records.
            wal.flush_through(wal.last_lsn()).unwrap();
            root // drop the tree/buffer/wal (crash: dirty pages never flushed)
        };

        // Recover the heap from the WAL - rebuilds the tree's pages from their
        // full-page-image records.
        let wal = Arc::new(Wal::open(&wal_path, wal_cfg).unwrap());
        {
            let disk = Disk::open(&heap_path, false).unwrap();
            recover(&wal, &disk).unwrap();
            disk.close().unwrap();
        }
        let disk = Arc::new(Disk::open(&heap_path, false).unwrap());
        let bp =
            Arc::new(BufferPool::new(disk, wal.clone(), BufConfig { frame_count: 16 }).unwrap());

        // Reopen at the fixed root - no rescan - and read everything back.
        let tree = BTree::open(bp, wal, root, 4);
        for i in 0..200u32 {
            assert_eq!(
                tree.search(&i.to_be_bytes()).unwrap(),
                Some(rid(i as u64)),
                "key {i} survived restart"
            );
        }
    }
}
