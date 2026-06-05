# Component: Disk Manager

**Crate:** `prism-storage`
**Status:** Accepted
**Last updated:** 2026-05-15

## Purpose

The disk manager owns the heap file. It provides page-grained read and write, controls `fsync` semantics, and isolates the rest of the engine from the OS file APIs. It does not interpret page contents; it does not cache; it does not retry. It is the thinnest possible abstraction over `read`, `write`, and `fsync`.

## Public interface

```rust
pub struct DiskManager {
    file: File,
    next_page_id: AtomicU64,
}

impl DiskManager {
    pub fn open(path: &Path, create: bool) -> Result<Self>;
    pub fn read_page(&self, page_id: PageId, buf: &mut [u8; PAGE_SIZE]) -> Result<()>;
    pub fn write_page(&self, page_id: PageId, buf: &[u8; PAGE_SIZE]) -> Result<()>;
    pub fn allocate_page(&self) -> Result<PageId>;
    pub fn sync(&self) -> Result<()>;
    pub fn page_count(&self) -> u64;
    pub fn close(self) -> Result<()>;
}
```

`PageId` is a `u64` (matches the 48-bit page field of `RecordId`; the upper 16 bits are always zero in v1, reserved for future tablespace ID).

`PAGE_SIZE` is the compile-time constant `8192`.

## Invariants

1. **Sized writes only.** `write_page` writes exactly `PAGE_SIZE` bytes at offset `page_id * PAGE_SIZE`. No partial writes from this layer.
2. **No torn write protection.** The disk manager assumes the OS and disk may tear an 8 KiB write into smaller atomic units. Higher layers (WAL, page checksums) detect torn writes.
3. **No retries.** I/O errors propagate up. The caller decides whether to retry, abort, or panic.
4. **Single owner.** One process opens one heap file. The disk manager takes an exclusive OS file lock to prevent concurrent opens — `flock` on Linux, `flock`/`fcntl` on macOS, `LockFileEx` (or an exclusive share mode) on Windows — and a second `open()` on a locked file returns `LockedByOtherProcess`.
5. **fsync is explicit.** `write_page` returns when the bytes have been handed to the OS, not when they are durable. Durability requires `sync()`.

## File layout

```
Offset 0:                  Page 0 (database metadata page)
Offset PAGE_SIZE:          Page 1 (catalog root)
Offset 2 * PAGE_SIZE:      Page 2 (free space map root)
Offset 3 * PAGE_SIZE:      Page 3 onward (heap pages, allocated as needed)
```

Page 0 contains the database header:
- Magic bytes: `PRISMDB\0` (8 bytes)
- Format version: `u32`
- Page size: `u32`
- Database creation timestamp: `i64`
- Last clean shutdown LSN: `u64`
- Reserved bytes to PAGE_SIZE

Page 0 is read at startup and its magic and version validated. Mismatch → `IncompatibleDatabase` error, refuse to open.

## I/O details

### Platform abstraction
Prism is a first-class citizen on Linux, macOS, and Windows. All OS-specific
file behavior is confined to one trait inside `prism-storage`; the rest of the
engine is portable Rust and never sees a platform `#[cfg]`. The trait abstracts
exactly three things — positioned read, positioned write, and durable sync —
plus open/lock/allocate. Each OS provides a backend, and every backend has a
portable buffered fallback so the engine runs even where the fast path is
unavailable (e.g. tmpfs rejecting direct I/O, or a filesystem without it).

| Operation | Linux | macOS | Windows |
|---|---|---|---|
| Positioned read | `pread` | `pread` | `ReadFile` + `OVERLAPPED` offset (or `seek_read`) |
| Positioned write | `pwrite` | `pwrite` | `WriteFile` + `OVERLAPPED` offset (or `seek_write`) |
| Bypass OS cache | `O_DIRECT` | `F_NOCACHE` | `FILE_FLAG_NO_BUFFERING` |
| Durable sync | `fdatasync` | `fcntl(F_FULLFSYNC)` | `FlushFileBuffers` (+ `FILE_FLAG_WRITE_THROUGH`) |
| Exclusive open | `flock` | `flock`/`fcntl` | `LockFileEx` / exclusive share mode |
| Preallocate | `fallocate` | `ftruncate` | `SetFileInformationByHandle` / `SetEndOfFile` |

Positioned `pread`/`pwrite` are thread-safe at the OS level on POSIX; on Windows,
positioned I/O via per-call `OVERLAPPED` offsets is likewise safe for concurrent
calls on the same handle without a shared file pointer.

### Direct I/O
Where supported, the file is opened to bypass the OS page cache (`O_DIRECT` on
Linux, `F_NOCACHE` on macOS, `FILE_FLAG_NO_BUFFERING` on Windows). Pages live in
our buffer pool, not in the kernel's. Rationale: the buffer pool already caches;
double-caching wastes memory and creates ambiguous fsync semantics.

If unbuffered I/O is not supported or is rejected by the filesystem, the disk
manager falls back to buffered I/O with explicit durable sync calls.

### Alignment
With `O_DIRECT`, buffers must be aligned (typically 4 KiB). The disk manager exposes `aligned_page_buffer()` for callers; the buffer pool's frames are allocated through this helper.

### Read path
1. `pread(fd, buf, PAGE_SIZE, page_id * PAGE_SIZE)`.
2. If returned bytes < PAGE_SIZE: `ShortRead` error. The page may exist but be partially written (a crash mid-write); higher layers handle.
3. If pread errors: propagate.

### Write path
1. `pwrite(fd, buf, PAGE_SIZE, page_id * PAGE_SIZE)`.
2. If returned bytes < PAGE_SIZE: `ShortWrite` error. The page is now in an indeterminate on-disk state; the caller must `sync()` and `read_page()` to learn what's there.
3. If pwrite errors: propagate.

### Sync
`sync()` issues the strongest durable-flush the OS offers: `fdatasync` on Linux,
`fcntl(F_FULLFSYNC)` on macOS, and `FlushFileBuffers` on Windows (paired with
`FILE_FLAG_WRITE_THROUGH` on the handle). The distinction matters: a plain
`fsync`/`FlushFileBuffers` may not force the *drive's* write cache to flush,
whereas `F_FULLFSYNC` (macOS) and write-through (Windows) do. We use the strong
form on every platform because durability under power loss matters more than a
few hundred microseconds. Where the strong form is unavailable, the disk manager
surfaces that at open time rather than silently weakening the durability promise.

## Page allocation

`allocate_page()` returns a `PageId`:
1. Acquires a coarse-grained mutex (allocation is not a hot path in practice).
2. Increments `next_page_id` atomically.
3. Extends the file via `fallocate` (Linux) or `ftruncate` (portable) to ensure space exists. This is rare; the file is preallocated in chunks of 64 pages.
4. Returns the new ID.

In v1, pages are never freed back to the OS. Deleted records leave free space within pages; new records reuse intra-page space. Page-level reuse is post-v1.

## Concurrency

The disk manager is `Send + Sync`. Multiple threads can call `read_page` and `write_page` concurrently. The underlying file descriptor's `pread`/`pwrite` are thread-safe at the OS level (POSIX guarantees this).

`fsync` is serialized through the WAL writer thread for the WAL files (see `components/wal.md`); for the heap file, `sync()` may be called from any thread, but the buffer pool's flush logic typically calls it from the page cleaner thread.

## Error catalog

| Error | Cause | Caller action |
|---|---|---|
| `LockedByOtherProcess` | Another Prism process has the file open | Fail startup with clear message |
| `IncompatibleDatabase` | Magic or version mismatch on page 0 | Fail startup |
| `ShortRead` | I/O returned < PAGE_SIZE bytes | Treat page as corrupted; rely on WAL redo to repair |
| `ShortWrite` | I/O returned < PAGE_SIZE bytes | Sync, retry; if persistent, abort with hard error |
| `Io(io::Error)` | OS-level I/O error | Generally fatal; log and abort |

## Testing strategy

- Unit tests for read/write round-trip.
- Property test: write random pages, sync, reopen, read; bytes match.
- Fault injection: a wrapper `DiskManager` that injects torn writes, short reads, and `EIO` errors at configurable probabilities. Used in the recovery test harness.
- Concurrent stress: 64 threads issuing random reads/writes; verify no corruption.

## Out of scope (v1)

- Multiple files per database (tablespaces). Reserved bits in `PageId` allow future expansion.
- File compression. Not appropriate at this layer.
- Encryption at rest. The operator's filesystem-level encryption is the answer for v1.
- Online file extension monitoring (free-space tracking is handled by the catalog).

## References

- ADR 0002 — page-based storage.
- ADR 0003 — WAL invariant requires durable sync semantics.
- `specs/page-format.md` — the byte layout the disk manager reads and writes.
