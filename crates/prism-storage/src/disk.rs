//! The disk manager and its cross-platform I/O backend.
//!
//! The disk manager owns the heap file and provides page-grained read/write
//! plus durable `sync`. It does not interpret page contents, cache, or retry.
//! See `docs/components/disk-manager.md`.
//!
//! All OS-specific behavior lives behind [`IoBackend`]. [`StdFileBackend`] is
//! the portable backend (positioned reads/writes via `std`, durable
//! `sync_data`), and works on Linux, macOS, and Windows. Faster per-OS backends
//! (direct I/O, `F_FULLFSYNC`, advisory locking) can be added without changing
//! the layers above.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Result, StorageError};
use crate::{PAGE_SIZE, PageId};

/// Pages are preallocated in chunks of this many to amortize file extension.
const PREALLOC_CHUNK_PAGES: u64 = 64;

/// The platform I/O surface the disk manager builds on.
///
/// Implementations provide positioned (offset-addressed) reads and writes,
/// durable sync, and file sizing. Positioned I/O is required so concurrent
/// callers never share a file cursor.
pub trait IoBackend: Send + Sync {
    /// Read into `buf` starting at byte `offset`. Returns bytes read (may be
    /// short at end-of-file).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    /// Write `buf` starting at byte `offset`. Returns bytes written.
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize>;
    /// Flush written bytes durably to the device (strongest available form).
    fn sync(&self) -> io::Result<()>;
    /// Set the file length to `len` bytes (used for preallocation).
    fn set_len(&self, len: u64) -> io::Result<()>;
    /// The current file length in bytes.
    fn size(&self) -> io::Result<u64>;
}

/// The portable, `std`-based backend. Works on every supported OS.
pub struct StdFileBackend {
    file: File,
}

impl StdFileBackend {
    /// Wrap an open file.
    pub fn new(file: File) -> Self {
        Self { file }
    }
}

impl IoBackend for StdFileBackend {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        positioned_read(&self.file, buf, offset)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        positioned_write(&self.file, buf, offset)
    }
    fn sync(&self) -> io::Result<()> {
        // Maps to fdatasync (Linux) / FlushFileBuffers (Windows) / fsync (macOS).
        // The stronger macOS F_FULLFSYNC is a hardening follow-up.
        self.file.sync_data()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }
    fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}

#[cfg(unix)]
fn positioned_read(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}
#[cfg(windows)]
fn positioned_read(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

#[cfg(unix)]
fn positioned_write(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}
#[cfg(windows)]
fn positioned_write(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

#[cfg(not(any(unix, windows)))]
compile_error!("prism-storage supports unix and windows targets only");

/// Owns the heap file and provides page-grained, positioned I/O.
pub struct DiskManager {
    backend: Box<dyn IoBackend>,
    next_page_id: AtomicU64,
    alloc_lock: Mutex<()>,
    path: Option<PathBuf>,
}

impl DiskManager {
    /// Open the heap file at `path`, creating it if `create` is set.
    ///
    /// The next page id is derived from the file length. This does not read or
    /// validate page 0; that is the caller's responsibility (see [`DbHeader`]).
    ///
    /// [`DbHeader`]: crate::DbHeader
    pub fn open(path: &Path, create: bool) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .open(path)?;
        let backend = StdFileBackend::new(file);
        let pages = backend.size()? / PAGE_SIZE as u64;
        Ok(Self {
            backend: Box::new(backend),
            next_page_id: AtomicU64::new(pages),
            alloc_lock: Mutex::new(()),
            path: Some(path.to_path_buf()),
        })
    }

    /// Construct a disk manager over an arbitrary backend (for tests and
    /// fault injection). `next_page_id` seeds the allocator.
    pub fn with_backend(backend: Box<dyn IoBackend>, next_page_id: u64) -> Self {
        Self {
            backend,
            next_page_id: AtomicU64::new(next_page_id),
            alloc_lock: Mutex::new(()),
            path: None,
        }
    }

    /// The file path, if this manager was opened from one.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Read `page_id` fully into `buf`.
    ///
    /// A read that returns fewer than `PAGE_SIZE` bytes (e.g. the page is past
    /// end-of-file, or a crash tore a write) yields [`StorageError::ShortRead`].
    pub fn read_page(&self, page_id: PageId, buf: &mut [u8; PAGE_SIZE]) -> Result<()> {
        let base = page_id.byte_offset();
        let mut done = 0usize;
        while done < PAGE_SIZE {
            let n = self.backend.read_at(base + done as u64, &mut buf[done..])?;
            if n == 0 {
                return Err(StorageError::ShortRead {
                    page: page_id.as_u64(),
                    got: done,
                    expected: PAGE_SIZE,
                });
            }
            done += n;
        }
        Ok(())
    }

    /// Write `buf` to `page_id`. Returns once the bytes are handed to the OS;
    /// durability requires a subsequent [`Self::sync`].
    pub fn write_page(&self, page_id: PageId, buf: &[u8; PAGE_SIZE]) -> Result<()> {
        let base = page_id.byte_offset();
        let mut done = 0usize;
        while done < PAGE_SIZE {
            let n = self.backend.write_at(base + done as u64, &buf[done..])?;
            if n == 0 {
                return Err(StorageError::ShortWrite {
                    page: page_id.as_u64(),
                    got: done,
                    expected: PAGE_SIZE,
                });
            }
            done += n;
        }
        Ok(())
    }

    /// Allocate a fresh page id, extending the file (in chunks) if needed.
    pub fn allocate_page(&self) -> Result<PageId> {
        let _guard = self.alloc_lock.lock().expect("alloc lock poisoned");
        let id = self.next_page_id.fetch_add(1, Ordering::SeqCst);
        let needed_pages = id + 1;
        let have_pages = self.backend.size()? / PAGE_SIZE as u64;
        if needed_pages > have_pages {
            let target_pages = needed_pages.div_ceil(PREALLOC_CHUNK_PAGES) * PREALLOC_CHUNK_PAGES;
            self.backend.set_len(target_pages * PAGE_SIZE as u64)?;
        }
        Ok(PageId(id))
    }

    /// The number of pages allocated so far.
    pub fn page_count(&self) -> u64 {
        self.next_page_id.load(Ordering::SeqCst)
    }

    /// Durably flush all prior writes to the device.
    pub fn sync(&self) -> Result<()> {
        self.backend.sync()?;
        Ok(())
    }

    /// Sync and close the heap file.
    pub fn close(self) -> Result<()> {
        self.backend.sync()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{PageType, SlottedPage};

    /// A self-deleting temporary file path.
    struct TempPath(PathBuf);

    impl TempPath {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let mut p = std::env::temp_dir();
            p.push(format!(
                "prism-storage-{tag}-{}-{n}-{nanos}.db",
                std::process::id()
            ));
            TempPath(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn page_filled(byte: u8) -> [u8; PAGE_SIZE] {
        [byte; PAGE_SIZE]
    }

    #[test]
    fn allocate_is_sequential_and_counts() {
        let tp = TempPath::new("alloc");
        let dm = DiskManager::open(tp.path(), true).unwrap();
        assert_eq!(dm.page_count(), 0);
        let ids: Vec<_> = (0..5).map(|_| dm.allocate_page().unwrap()).collect();
        assert_eq!(ids, (0..5).map(PageId).collect::<Vec<_>>());
        assert_eq!(dm.page_count(), 5);
    }

    #[test]
    fn write_sync_reopen_read_roundtrip() {
        let tp = TempPath::new("rt");
        {
            let dm = DiskManager::open(tp.path(), true).unwrap();
            for i in 0..5u64 {
                let id = dm.allocate_page().unwrap();
                dm.write_page(id, &page_filled(i as u8)).unwrap();
            }
            dm.close().unwrap();
        }
        // Reopen and verify every page survived.
        let dm = DiskManager::open(tp.path(), false).unwrap();
        for i in 0..5u64 {
            let mut buf = [0u8; PAGE_SIZE];
            dm.read_page(PageId(i), &mut buf).unwrap();
            assert_eq!(buf, page_filled(i as u8), "page {i} mismatch after reopen");
        }
    }

    #[test]
    fn reading_unallocated_page_is_short_read() {
        let tp = TempPath::new("short");
        let dm = DiskManager::open(tp.path(), true).unwrap();
        let id = dm.allocate_page().unwrap();
        dm.write_page(id, &page_filled(7)).unwrap();
        dm.sync().unwrap();
        // A page id well past the written region: the file is preallocated to a
        // chunk boundary, so a read far beyond it must be short.
        let mut buf = [0u8; PAGE_SIZE];
        let err = dm.read_page(PageId(PREALLOC_CHUNK_PAGES + 1), &mut buf);
        assert!(
            matches!(err, Err(StorageError::ShortRead { .. })),
            "{err:?}"
        );
    }

    #[test]
    fn slotted_page_survives_disk_roundtrip() {
        let tp = TempPath::new("slotted");
        let id;
        {
            let dm = DiskManager::open(tp.path(), true).unwrap();
            id = dm.allocate_page().unwrap();
            let mut buf = [0u8; PAGE_SIZE];
            let mut page = SlottedPage::init(&mut buf, PageType::Heap);
            page.insert(b"the quick brown fox").unwrap();
            page.insert(b"jumps over").unwrap();
            page.update_checksum();
            dm.write_page(id, &buf).unwrap();
            dm.close().unwrap();
        }
        let dm = DiskManager::open(tp.path(), false).unwrap();
        let mut buf = [0u8; PAGE_SIZE];
        dm.read_page(id, &mut buf).unwrap();
        let page = SlottedPage::new(&mut buf);
        assert!(page.verify_checksum());
        assert_eq!(page.get(0), Some(&b"the quick brown fox"[..]));
        assert_eq!(page.get(1), Some(&b"jumps over"[..]));
    }
}
