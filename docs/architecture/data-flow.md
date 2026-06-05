# Data Flow

**Status:** Accepted
**Last updated:** 2026-05-15

This document traces the path of a request through the system, from the client to disk and back. The architecture document (`system-architecture.md`) describes the components; this document describes how a single operation flows across them.

## Setup: an implicit single-statement transaction

A client sends a SQL insert: `INSERT INTO users (id, email) VALUES (1, 'a@b.com')`.

### 1. Network arrival
- The TCP server receives bytes on the connection.
- Length-prefix decoder assembles a complete frame.
- The frame is decoded into a `Request::Query { sql: "...", txn: None }`.
- The dispatcher sees `txn: None` and decides this is an implicit transaction.

### 2. Transaction begin
- Dispatcher calls `txn_manager.begin()`.
- Transaction manager allocates `TxnId = 42`, inserts into the active transaction table with `state = ACTIVE`, snapshot = current commit horizon.
- Returns a `TxnHandle` carrying TxnId 42 and the snapshot.

### 3. SQL parsing and planning
- SQL engine parses `INSERT INTO users ...` into an AST.
- AST is bound to the catalog: `users` resolves to table OID 12, columns to (column_id 1, INT), (column_id 2, TEXT).
- A logical plan is built: `Insert(users, [(1, 'a@b.com')])`.
- Physical plan: `InsertOp(table=12, values=[1, "a@b.com"], indexes=[pk_idx, email_idx])`.

### 4. Execution
- Executor invokes the insert operator.
- Operator serializes the row into the tuple format defined in `specs/record-format.md`.
- Operator calls `record_store.insert(txn=42, bytes=row_bytes, hint=Heap(table_12))`.

### 5. Record store insert
- Record store determines the target heap by table OID.
- Calls `buffer_pool.fetch_writable_page(Heap(table_12).current_free_page)`.
- Buffer pool returns a pinned, writable page handle.
- Record store sets `xmin = 42`, `xmax = 0` on the tuple header.
- Record store finds a free slot in the page, copies the tuple into the slot.

### 6. WAL record before page write
- Before the page modification is "official," record store calls `wal.append(LogRecord::Insert { txn: 42, page: P, slot: S, before: empty, after: tuple_bytes })`.
- WAL allocates LSN 9001, appends to the in-memory buffer.
- Record store sets the page's `page_lsn = 9001`.
- The page is now dirty but not flushed; the WAL record is in the WAL buffer but not fsync'd.

### 7. Index updates
- For each index on the table, record store calls into the access method.
- Primary key B+tree: insert key `1 -> RID(P, S)`. This itself produces a WAL record (LSN 9002) and modifies a B+tree leaf page.
- Email index B+tree: insert key `"a@b.com" -> RID(P, S)`. WAL record LSN 9003.

### 8. Commit
- Executor returns success to dispatcher.
- Dispatcher calls `txn_manager.commit(42)`.
- Transaction manager writes `LogRecord::Commit { txn: 42 }` at LSN 9004.
- Transaction manager calls `wal.flush_through(9004)`. This is the synchronous fsync; the call blocks until the WAL is durable up through LSN 9004.
- Group commit may batch this with other in-flight commits.
- Once fsync returns, transaction manager updates the commit log: `txn 42 -> COMMITTED at LSN 9004`.
- Transaction manager removes txn 42 from the active transaction table.

### 9. Response
- Dispatcher builds `Response::QueryOk { rows_affected: 1 }`.
- Network server frames and writes to the TCP connection.
- Client receives the response.

### 10. Asynchronous: dirty page flushing
- Pages P (heap), PK leaf, email leaf are all dirty in the buffer pool.
- They will be flushed lazily by the background page cleaner or eagerly under buffer pool pressure.
- Critically, the WAL records (LSN 9001-9004) are already on disk. The dirty pages are not yet on disk, but recovery can reconstruct them if we crash now.

## An explicit cross-model transaction

A client sends:
```
BEGIN
  INSERT INTO orders ... (SQL)
  db.events.insertOne({ order_id: ... }) (document)
  PUT session:abc = "valid" (KV)
COMMIT
```

The flow is identical except:

- Steps 2-7 happen three times, once per operation, but with the **same** TxnId across all three.
- The dispatcher routes each operation to its own engine (SQL, document, KV).
- The document insert calls `record_store.insert(txn=42, bytes=doc_bytes, hint=Heap(collection_events))`.
- The KV put calls `record_store.insert(txn=42, bytes=kv_bytes, hint=Heap(namespace_session))` and updates the namespace's hash index.
- Step 8 (commit) writes one commit record. The WAL contains insert records for all three operations under TxnId 42, all of which become visible simultaneously when the commit record is durable.

This is the cross-model property in operation: nothing in the record store, buffer pool, or WAL has any special handling for cross-model transactions. They are just transactions.

## A read

A client sends `SELECT * FROM users WHERE id = 1`.

### 1. Routing and parsing
- Identical to the insert path through step 3.
- Physical plan: `IndexScan(pk_idx, range=[1, 1])` → `ProjectAll`.

### 2. Snapshot
- Transaction begins (implicit), gets TxnId 43 and snapshot S = 42 (the most recent committed TxnId).

### 3. Index lookup
- IndexScan asks the B+tree for key 1.
- B+tree returns `RID(P, S)`.

### 4. Record fetch with visibility
- Record store calls `buffer_pool.fetch_readable_page(P)`.
- Reads the tuple at slot S.
- Checks visibility: `xmin = 42`, `xmax = 0`. Is xmin 42 visible to snapshot 43? Check commit log: txn 42 is COMMITTED. Is 42 <= 43? Yes. Is xmax 0 (current)? Yes. Tuple is visible.
- Returns tuple bytes.

### 5. Version chain walk (the interesting case)
- Suppose another transaction had updated the same row before we started.
- Record store reads tuple version V1 at RID. `V1.xmax = 50` (deleted by txn 50).
- Is txn 50 visible to our snapshot 43? No, 50 > 43. So V1 is still visible to us.
- Continue: V1.xmin = 42, committed, 42 <= 43. Visible. Return V1.

If V1.xmax = 30 instead (deleted by txn 30, which is < 43 and committed):
- V1 is not visible to us anymore.
- Follow V1's version chain pointer to V0.
- V0.xmin = 10, V0.xmax = 30. Was 10 committed and <= 43? Yes. Is xmax 30 visible to us? Yes, 30 <= 43, committed. So V0 was deleted before our snapshot. No version visible.
- Return None.

### 6. Result projection and response
- Executor builds the result rows.
- Dispatcher frames `Response::Rows { columns, rows }`.
- Network server writes.

## An aborted transaction

A client sends:
```
BEGIN
INSERT INTO users ...
ROLLBACK
```

### 1-7. Same as insert
- Logs and pages are modified as usual.
- WAL records LSN 9001 (insert), 9002 (PK index), 9003 (email index) are in the WAL buffer.

### 8. Rollback
- Dispatcher calls `txn_manager.abort(42)`.
- Transaction manager writes `LogRecord::Abort { txn: 42 }` at LSN 9004.
- For each modification by txn 42, writes a Compensation Log Record (CLR):
  - LSN 9005: CLR undoing email index insert
  - LSN 9006: CLR undoing PK index insert
  - LSN 9007: CLR undoing heap insert (marks the tuple `xmax = 42`, which makes it invisible to anyone)
- Updates commit log: txn 42 -> ABORTED.

### 9. Visibility after abort
- The tuple physically still exists in the page.
- Any reader checking visibility sees `xmin = 42`. Looks up txn 42 in commit log: ABORTED. Tuple is not visible.
- The space will be reclaimed eventually by a vacuum process (not implemented in v1; the space leaks until then).

## A crash

Suppose the system crashes between steps 7 and 8 of the original insert example.
- WAL records 9001-9003 may or may not be on disk depending on whether fsync was called for them. By the WAL invariant, the corresponding pages cannot have been flushed unless the log was. So either: (a) the log is on disk but no commit record exists, in which case recovery undoes the partial txn, or (b) the log is not on disk and the pages aren't either, in which case the transaction simply never happened.

If the crash is between 8 (fsync return) and 9 (response send):
- The commit record is on disk.
- The client never received the response.
- The client must retry; the operation may be replayed. This is why every external request carries an idempotency key — see the SDK API spec.

Recovery details are in `components/recovery.md`.

## What this flow leaves out

- Lock acquisition for writers (covered in `components/lock-manager.md`)
- Deadlock detection (`components/lock-manager.md`)
- Index page splits (`components/btree.md`)
- Checkpointing (`components/wal.md`)
- Vacuum / version chain pruning (deferred to post-v1)

This document is meant to make the happy paths concrete enough to argue about. The pathological paths are in the per-component documents.
