//! `prism-doc` — the document engine.
//!
//! Schemaless documents stored as self-describing tagged binary in a collection
//! (a heap of records), with a MongoDB-subset query language. CRUD goes through
//! the unified record store, so documents share MVCC, locking, recovery, and
//! cross-model transactions with SQL and KV. See `docs/components/document-engine.md`.
//!
//! **Scope (this slice):** CRUD (`insert`/`find`/`update`/`delete`) with
//! programmatically-built [`Filter`]s (eq/ne/gt/lt/gte/lte/in/nin/exists/and/or/
//! not) and [`Update`] operators (`$set`/`$unset`/`$inc`) on top-level scalar
//! fields. Every collection has a durable B+tree index on `_id` (unique), so an
//! `_id`-equality filter is an index seek; other filters use a sequential scan.
//! Deferred: nested/dotted paths, arrays/objects, secondary field-path indexes,
//! and a JSON query parser (queries are built in Rust).

pub mod error;
pub mod value;

pub use error::{DocError, Result};
pub use value::{DocValue, Document, ObjectId, doc_cmp};

use std::cmp::Ordering;
use std::sync::Arc;

use prism_core::RecordId;
use prism_core::store::{HeapId, RecordStore};
use prism_core::txn::TxnHandle;
use prism_index::BTree;
use prism_storage::PageId;

/// A query predicate over top-level fields.
#[derive(Clone, Debug)]
pub enum Filter {
    /// Matches every document (the empty query `{}`).
    All,
    /// `field == value` (numeric across int/double; `Null` also matches missing).
    Eq(String, DocValue),
    /// `field != value`.
    Ne(String, DocValue),
    /// `field > value`.
    Gt(String, DocValue),
    /// `field < value`.
    Lt(String, DocValue),
    /// `field >= value`.
    Gte(String, DocValue),
    /// `field <= value`.
    Lte(String, DocValue),
    /// `field` in the set.
    In(String, Vec<DocValue>),
    /// `field` not in the set (also matches when the field is missing).
    Nin(String, Vec<DocValue>),
    /// Whether `field` exists.
    Exists(String, bool),
    /// All sub-filters match.
    And(Vec<Filter>),
    /// Any sub-filter matches.
    Or(Vec<Filter>),
    /// The sub-filter does not match.
    Not(Box<Filter>),
}

impl Filter {
    /// Evaluate this filter against `doc`.
    pub fn matches(&self, doc: &Document) -> bool {
        match self {
            Filter::All => true,
            Filter::Eq(f, v) => match doc.get(f) {
                Some(found) => doc_cmp(found, v) == Ordering::Equal,
                None => matches!(v, DocValue::Null),
            },
            Filter::Ne(f, v) => !Filter::Eq(f.clone(), v.clone()).matches(doc),
            Filter::Gt(f, v) => cmp_field(doc, f, v) == Some(Ordering::Greater),
            Filter::Lt(f, v) => cmp_field(doc, f, v) == Some(Ordering::Less),
            Filter::Gte(f, v) => matches!(
                cmp_field(doc, f, v),
                Some(Ordering::Greater | Ordering::Equal)
            ),
            Filter::Lte(f, v) => {
                matches!(cmp_field(doc, f, v), Some(Ordering::Less | Ordering::Equal))
            }
            Filter::In(f, set) => doc
                .get(f)
                .is_some_and(|found| set.iter().any(|v| doc_cmp(found, v) == Ordering::Equal)),
            Filter::Nin(f, set) => match doc.get(f) {
                None => true,
                Some(found) => !set.iter().any(|v| doc_cmp(found, v) == Ordering::Equal),
            },
            Filter::Exists(f, want) => doc.contains(f) == *want,
            Filter::And(subs) => subs.iter().all(|s| s.matches(doc)),
            Filter::Or(subs) => subs.iter().any(|s| s.matches(doc)),
            Filter::Not(inner) => !inner.matches(doc),
        }
    }
}

fn cmp_field(doc: &Document, field: &str, v: &DocValue) -> Option<Ordering> {
    doc.get(field).map(|found| doc_cmp(found, v))
}

/// A mutation: a sequence of update operators applied in order.
#[derive(Clone, Debug, Default)]
pub struct Update {
    ops: Vec<UpdateOp>,
}

#[derive(Clone, Debug)]
enum UpdateOp {
    Set(String, DocValue),
    Unset(String),
    Inc(String, i64),
}

impl Update {
    /// An empty update.
    pub fn new() -> Self {
        Self::default()
    }
    /// `$set field = value`.
    pub fn set(mut self, field: impl Into<String>, value: DocValue) -> Self {
        self.ops.push(UpdateOp::Set(field.into(), value));
        self
    }
    /// `$unset field`.
    pub fn unset(mut self, field: impl Into<String>) -> Self {
        self.ops.push(UpdateOp::Unset(field.into()));
        self
    }
    /// `$inc field by delta` (integer add; creates the field if missing).
    pub fn inc(mut self, field: impl Into<String>, delta: i64) -> Self {
        self.ops.push(UpdateOp::Inc(field.into(), delta));
        self
    }

    fn apply(&self, doc: &mut Document) {
        for op in &self.ops {
            match op {
                UpdateOp::Set(f, v) => {
                    doc.set(f.clone(), v.clone());
                }
                UpdateOp::Unset(f) => {
                    doc.remove(f);
                }
                UpdateOp::Inc(f, delta) => {
                    let current = match doc.get(f) {
                        Some(DocValue::Int64(n)) => *n,
                        Some(DocValue::Int32(n)) => *n as i64,
                        _ => 0,
                    };
                    doc.set(f.clone(), DocValue::Int64(current + delta));
                }
            }
        }
    }
}

/// A collection of documents backed by one heap, with a durable B+tree index on
/// `_id` (the document primary key): `find`/`update`/`delete` with an
/// `_id`-equality filter is an index seek instead of a full scan, and `_id` is
/// unique. Other filters fall back to a sequential scan.
pub struct DocCollection {
    store: Arc<RecordStore>,
    heap: HeapId,
    id_index_root: PageId,
}

impl DocCollection {
    /// Create a collection backed by `heap`, building a fresh `_id` index.
    pub fn create(store: Arc<RecordStore>, heap: HeapId) -> Result<Self> {
        let tree = BTree::create(store.buffer(), store.wal())?;
        let id_index_root = tree.root_page();
        Ok(Self {
            store,
            heap,
            id_index_root,
        })
    }

    /// Reopen a collection whose `_id` index is rooted at `index_root`.
    pub fn open(store: Arc<RecordStore>, heap: HeapId, index_root: PageId) -> Self {
        Self {
            store,
            heap,
            id_index_root: index_root,
        }
    }

    /// The `_id` index tree's (fixed) root page, for persistence by the catalog.
    pub fn index_root(&self) -> PageId {
        self.id_index_root
    }

    fn id_index(&self) -> BTree {
        BTree::open(
            self.store.buffer(),
            self.store.wal(),
            self.id_index_root,
            usize::MAX,
        )
    }

    /// Insert `doc`, assigning an `_id` `ObjectId` if absent. Returns the `_id`.
    /// Rejects a duplicate `_id` visible to `txn`.
    pub fn insert_one(&self, txn: &TxnHandle, mut doc: Document) -> Result<DocValue> {
        let id = match doc.get("_id") {
            Some(existing) => existing.clone(),
            None => {
                let id = DocValue::ObjectId(ObjectId::generate());
                doc.set_front("_id", id.clone());
                id
            }
        };
        let key = index_key(&id)?;
        let tree = self.id_index();
        if let Some(existing) = tree.search(&key)? {
            if self.store.read(txn, existing)?.is_some() {
                return Err(DocError::Constraint("duplicate _id".into()));
            }
        }
        let rid = self.store.insert(txn, self.heap, &doc.encode()?)?;
        tree.insert(&key, rid)?;
        Ok(id)
    }

    /// Insert several documents, returning their `_id`s.
    pub fn insert_many(&self, txn: &TxnHandle, docs: Vec<Document>) -> Result<Vec<DocValue>> {
        docs.into_iter().map(|d| self.insert_one(txn, d)).collect()
    }

    /// All documents matching `filter`, visible to `txn`.
    pub fn find(&self, txn: &TxnHandle, filter: &Filter) -> Result<Vec<Document>> {
        if let Some(id) = eq_id(filter) {
            return Ok(self.seek_id(txn, id)?.map(|(_, d)| d).into_iter().collect());
        }
        let mut out = Vec::new();
        for (_, payload) in self.store.scan(txn, self.heap)? {
            let doc = Document::decode(&payload)?;
            if filter.matches(&doc) {
                out.push(doc);
            }
        }
        Ok(out)
    }

    /// The first document matching `filter`.
    pub fn find_one(&self, txn: &TxnHandle, filter: &Filter) -> Result<Option<Document>> {
        if let Some(id) = eq_id(filter) {
            return Ok(self.seek_id(txn, id)?.map(|(_, d)| d));
        }
        for (_, payload) in self.store.scan(txn, self.heap)? {
            let doc = Document::decode(&payload)?;
            if filter.matches(&doc) {
                return Ok(Some(doc));
            }
        }
        Ok(None)
    }

    /// Seek the (unique) document whose `_id == id`, visible to `txn`.
    fn seek_id(&self, txn: &TxnHandle, id: &DocValue) -> Result<Option<(RecordId, Document)>> {
        let Some(rid) = self.id_index().search(&index_key(id)?)? else {
            return Ok(None);
        };
        match self.store.read(txn, rid)? {
            Some(payload) => Ok(Some((rid, Document::decode(&payload)?))),
            None => Ok(None),
        }
    }

    /// Apply `update` to documents matching `filter`. Returns the count modified.
    /// `one` limits to the first match.
    fn update_internal(
        &self,
        txn: &TxnHandle,
        filter: &Filter,
        update: &Update,
        one: bool,
    ) -> Result<u64> {
        let tree = self.id_index();

        // `_id` seek: at most one document.
        if let Some(id) = eq_id(filter) {
            return match self.seek_id(txn, id)? {
                Some((rid, mut doc)) => {
                    update.apply(&mut doc);
                    let new_rid = self.store.update(txn, rid, &doc.encode()?)?;
                    self.reindex(&tree, &doc, new_rid)?;
                    Ok(1)
                }
                None => Ok(0),
            };
        }

        let mut modified = 0;
        for (rid, payload) in self.store.scan(txn, self.heap)? {
            let mut doc = Document::decode(&payload)?;
            if !filter.matches(&doc) {
                continue;
            }
            update.apply(&mut doc);
            let new_rid = self.store.update(txn, rid, &doc.encode()?)?;
            self.reindex(&tree, &doc, new_rid)?;
            modified += 1;
            if one {
                break;
            }
        }
        Ok(modified)
    }

    /// Repoint the `_id` index at a document's new version after an update.
    fn reindex(&self, tree: &BTree, doc: &Document, new_rid: RecordId) -> Result<()> {
        if let Some(id) = doc.get("_id") {
            tree.insert(&index_key(id)?, new_rid)?;
        }
        Ok(())
    }

    /// Update the first matching document. Returns the count modified (0 or 1).
    pub fn update_one(&self, txn: &TxnHandle, filter: &Filter, update: &Update) -> Result<u64> {
        self.update_internal(txn, filter, update, true)
    }

    /// Update all matching documents. Returns the count modified.
    pub fn update_many(&self, txn: &TxnHandle, filter: &Filter, update: &Update) -> Result<u64> {
        self.update_internal(txn, filter, update, false)
    }

    fn delete_internal(&self, txn: &TxnHandle, filter: &Filter, one: bool) -> Result<u64> {
        // `_id` seek: at most one document. The index entry is left in place; MVCC
        // hides the deleted version from new snapshots (as in the KV engine).
        if let Some(id) = eq_id(filter) {
            return match self.seek_id(txn, id)? {
                Some((rid, _)) => {
                    self.store.delete(txn, rid)?;
                    Ok(1)
                }
                None => Ok(0),
            };
        }

        let mut deleted = 0;
        for (rid, payload) in self.store.scan(txn, self.heap)? {
            let doc = Document::decode(&payload)?;
            if !filter.matches(&doc) {
                continue;
            }
            self.store.delete(txn, rid)?;
            deleted += 1;
            if one {
                break;
            }
        }
        Ok(deleted)
    }

    /// Delete the first matching document. Returns the count deleted (0 or 1).
    pub fn delete_one(&self, txn: &TxnHandle, filter: &Filter) -> Result<u64> {
        self.delete_internal(txn, filter, true)
    }

    /// Delete all matching documents. Returns the count deleted.
    pub fn delete_many(&self, txn: &TxnHandle, filter: &Filter) -> Result<u64> {
        self.delete_internal(txn, filter, false)
    }
}

/// If `filter` is exactly `_id == <value>`, return that value for an index seek.
fn eq_id(filter: &Filter) -> Option<&DocValue> {
    match filter {
        Filter::Eq(field, value) if field == "_id" => Some(value),
        _ => None,
    }
}

/// Encode a document value as a byte key for the `_id` B+tree, tagged by type so
/// values of different types never collide. Integers are sign-flipped big-endian
/// (order-preserving).
fn index_key(value: &DocValue) -> Result<Vec<u8>> {
    let mut key = Vec::with_capacity(16);
    match value {
        DocValue::Null => return Err(DocError::Constraint("_id cannot be null".into())),
        DocValue::Bool(b) => {
            key.push(0x01);
            key.push(u8::from(*b));
        }
        DocValue::Int32(n) => {
            key.push(0x02);
            key.extend_from_slice(&(*n as u32 ^ (1u32 << 31)).to_be_bytes());
        }
        DocValue::Int64(n) => {
            key.push(0x03);
            key.extend_from_slice(&(*n as u64 ^ (1u64 << 63)).to_be_bytes());
        }
        DocValue::Double(d) => {
            key.push(0x04);
            key.extend_from_slice(&d.to_be_bytes());
        }
        DocValue::Str(s) => {
            key.push(0x05);
            key.extend_from_slice(s.as_bytes());
        }
        DocValue::Timestamp(t) => {
            key.push(0x09);
            key.extend_from_slice(&(*t as u64 ^ (1u64 << 63)).to_be_bytes());
        }
        DocValue::ObjectId(oid) => {
            key.push(0x0A);
            key.extend_from_slice(&oid.0);
        }
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_buffer::{BufferPool, Config as BufConfig};
    use prism_core::txn::{TxnManager, TxnMode};
    use prism_storage::DiskManager;
    use prism_testkit::TempDir;
    use prism_wal::{Config as WalConfig, SyncMode, Wal};

    struct Env {
        coll: DocCollection,
        txns: Arc<TxnManager>,
        _tmp: TempDir,
    }

    fn env() -> Env {
        let tmp = TempDir::new("doc").unwrap();
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
        let buffer =
            Arc::new(BufferPool::new(disk, wal.clone(), BufConfig { frame_count: 32 }).unwrap());
        let txns = Arc::new(TxnManager::new(wal.clone()));
        let store = Arc::new(RecordStore::new(buffer, wal, txns.clone()));
        Env {
            coll: DocCollection::create(store, HeapId(5000)).unwrap(),
            txns,
            _tmp: tmp,
        }
    }

    fn doc(fields: &[(&str, DocValue)]) -> Document {
        Document::from_fields(fields.iter().map(|(k, v)| (k.to_string(), v.clone())))
    }

    #[test]
    fn insert_assigns_id_and_find_returns_it() {
        let env = env();
        let t = env.txns.begin(TxnMode::ReadWrite);
        let id = env
            .coll
            .insert_one(&t, doc(&[("name", DocValue::Str("alice".into()))]))
            .unwrap();
        assert!(matches!(id, DocValue::ObjectId(_)));
        let found = env.coll.find(&t, &Filter::All).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].get("name"), Some(&DocValue::Str("alice".into())));
        assert_eq!(found[0].get("_id"), Some(&id));
        t.commit().unwrap();
    }

    #[test]
    fn id_index_seek_uniqueness_and_reindex() {
        let env = env();
        let t = env.txns.begin(TxnMode::ReadWrite);
        let id1 = env
            .coll
            .insert_one(&t, doc(&[("name", DocValue::Str("a".into()))]))
            .unwrap();
        env.coll
            .insert_one(&t, doc(&[("name", DocValue::Str("b".into()))]))
            .unwrap();

        // Seek by _id returns exactly that document.
        let found = env
            .coll
            .find(&t, &Filter::Eq("_id".into(), id1.clone()))
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].get("name"), Some(&DocValue::Str("a".into())));
        // A missing _id finds nothing.
        assert!(
            env.coll
                .find(&t, &Filter::Eq("_id".into(), DocValue::Int64(999)))
                .unwrap()
                .is_empty()
        );

        // A custom, explicit _id is honored and must be unique.
        let custom = DocValue::Int64(42);
        env.coll
            .insert_one(
                &t,
                doc(&[("_id", custom.clone()), ("x", DocValue::Int64(1))]),
            )
            .unwrap();
        assert!(matches!(
            env.coll.insert_one(
                &t,
                doc(&[("_id", custom.clone()), ("x", DocValue::Int64(2))])
            ),
            Err(DocError::Constraint(_))
        ));

        // Update via _id seek repoints the index at the new version.
        let n = env
            .coll
            .update_one(
                &t,
                &Filter::Eq("_id".into(), custom.clone()),
                &Update::new().set("x", DocValue::Int64(99)),
            )
            .unwrap();
        assert_eq!(n, 1);
        let after = env
            .coll
            .find_one(&t, &Filter::Eq("_id".into(), custom.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(after.get("x"), Some(&DocValue::Int64(99)));

        // Delete via _id seek, then the seek finds nothing.
        assert_eq!(
            env.coll
                .delete_one(&t, &Filter::Eq("_id".into(), id1.clone()))
                .unwrap(),
            1
        );
        assert!(
            env.coll
                .find(&t, &Filter::Eq("_id".into(), id1))
                .unwrap()
                .is_empty()
        );
        t.commit().unwrap();
    }

    #[test]
    fn filters_select_correctly() {
        let env = env();
        let t = env.txns.begin(TxnMode::ReadWrite);
        env.coll
            .insert_many(
                &t,
                vec![
                    doc(&[
                        ("n", DocValue::Int64(1)),
                        ("city", DocValue::Str("NYC".into())),
                    ]),
                    doc(&[
                        ("n", DocValue::Int64(2)),
                        ("city", DocValue::Str("LA".into())),
                    ]),
                    doc(&[
                        ("n", DocValue::Int64(3)),
                        ("city", DocValue::Str("NYC".into())),
                    ]),
                ],
            )
            .unwrap();

        let f = Filter::And(vec![
            Filter::Gt("n".into(), DocValue::Int64(1)),
            Filter::Eq("city".into(), DocValue::Str("NYC".into())),
        ]);
        let got = env.coll.find(&t, &f).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].get("n"), Some(&DocValue::Int64(3)));

        // $in and $exists
        let f = Filter::In("n".into(), vec![DocValue::Int64(1), DocValue::Int64(3)]);
        assert_eq!(env.coll.find(&t, &f).unwrap().len(), 2);
        let f = Filter::Exists("city".into(), true);
        assert_eq!(env.coll.find(&t, &f).unwrap().len(), 3);
        t.commit().unwrap();
    }

    #[test]
    fn update_and_delete() {
        let env = env();
        let t = env.txns.begin(TxnMode::ReadWrite);
        env.coll
            .insert_many(
                &t,
                vec![
                    doc(&[
                        ("k", DocValue::Str("a".into())),
                        ("hits", DocValue::Int64(0)),
                    ]),
                    doc(&[
                        ("k", DocValue::Str("b".into())),
                        ("hits", DocValue::Int64(0)),
                    ]),
                ],
            )
            .unwrap();

        let n = env
            .coll
            .update_one(
                &t,
                &Filter::Eq("k".into(), DocValue::Str("a".into())),
                &Update::new()
                    .inc("hits", 5)
                    .set("seen", DocValue::Bool(true)),
            )
            .unwrap();
        assert_eq!(n, 1);
        let a = env
            .coll
            .find_one(&t, &Filter::Eq("k".into(), DocValue::Str("a".into())))
            .unwrap()
            .unwrap();
        assert_eq!(a.get("hits"), Some(&DocValue::Int64(5)));
        assert_eq!(a.get("seen"), Some(&DocValue::Bool(true)));

        let d = env
            .coll
            .delete_one(&t, &Filter::Eq("k".into(), DocValue::Str("b".into())))
            .unwrap();
        assert_eq!(d, 1);
        assert_eq!(env.coll.find(&t, &Filter::All).unwrap().len(), 1);
        t.commit().unwrap();
    }

    #[test]
    fn respects_snapshot_isolation() {
        let env = env();
        // Committed insert by one txn.
        let w = env.txns.begin(TxnMode::ReadWrite);
        env.coll
            .insert_one(&w, doc(&[("v", DocValue::Int64(1))]))
            .unwrap();

        let early = env.txns.begin(TxnMode::ReadOnly); // begins before commit
        w.commit().unwrap();

        // The early reader's snapshot predates the commit.
        assert_eq!(env.coll.find(&early, &Filter::All).unwrap().len(), 0);
        early.commit().unwrap();

        let late = env.txns.begin(TxnMode::ReadOnly);
        assert_eq!(env.coll.find(&late, &Filter::All).unwrap().len(), 1);
        late.commit().unwrap();
    }
}
