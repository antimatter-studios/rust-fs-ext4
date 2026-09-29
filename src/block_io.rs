//! Abstract block-device I/O.
//!
//! The driver doesn't care if blocks come from a file, raw device, or a
//! callback into Swift — it just needs `read_at(offset, buf) -> Result<()>`.
//!
//! `write_at` is an optional trait method: it defaults to returning
//! `Error::Corrupt("read-only device")` so every existing read-only caller
//! keeps working. `FileDevice` and the callback-with-writer device override
//! it when the underlying resource allows writes.

use crate::error::{Error, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

/// Random-access block device. Reads required; writes optional.
pub trait BlockDevice: Send + Sync {
    /// Read exactly `buf.len()` bytes starting at `offset` (bytes from start of device).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Total device size in bytes (for bounds-checking).
    fn size_bytes(&self) -> u64;

    /// Write exactly `buf.len()` bytes at `offset`. Default: returns an error
    /// for read-only devices. Writable devices override this.
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<()> {
        Err(Error::Corrupt(
            "block device is read-only (no write_at impl)",
        ))
    }

    /// Flush any pending writes to stable storage. Default: no-op for
    /// read-only devices; writable devices should implement fsync semantics.
    fn flush(&self) -> Result<()> {
        Ok(())
    }

    /// Reports whether `write_at` is likely to succeed. Used by the mount
    /// path to decide whether journal replay is possible.
    fn is_writable(&self) -> bool {
        false
    }

    /// Buffer-cache hook: stash `bytes` for `block` so a subsequent
    /// `read_at` returns those bytes instead of reading from physical
    /// storage. Used by a read-only mount's journal replay to make
    /// committed metadata visible to readers without writing it back to
    /// the data area on disk.
    ///
    /// Pinned entries inserted via `populate_cache` MUST NOT be evicted
    /// — they're the only place those bytes exist until `unpin_all`
    /// runs (typically after journal replay). Devices without a cache
    /// (raw `FileDevice`, etc.) implement this as a no-op and the
    /// caller's bytes simply have no in-memory shadow; that's safe
    /// because un-cached devices imply no separate journal log either.
    fn populate_cache(&self, _block: u64, _bytes: Vec<u8>) {}

    /// Buffer-cache hook: tell the device the journal has been
    /// checkpointed, so any blocks pinned via `populate_cache` are now
    /// consistent with disk and can be evicted under normal LRU
    /// pressure. No-op for un-cached devices.
    fn unpin_all(&self) {}

    /// Discard clean read-cache entries. Pinned journal metadata must be
    /// checkpointed and unpinned first; cached implementations reject otherwise.
    /// This does not flush writes and requires exclusive filesystem ownership.
    fn invalidate_cache(&self) -> Result<()> {
        Ok(())
    }
}

/// File-backed device — used for disk images and `/dev/diskN`.
pub struct FileDevice {
    file: Mutex<File>,
    size: u64,
    writable: bool,
}

impl FileDevice {
    /// Open read-only. Matches pre-existing behaviour.
    pub fn open(path: &str) -> Result<Self> {
        Self::open_path(Path::new(path))
    }

    /// Open read-only by a path that need not be UTF-8: a device or image
    /// name is bytes on Unix, and a lossy rendering of it can name another
    /// file.
    pub fn open_path(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file: Mutex::new(file),
            size,
            writable: false,
        })
    }

    /// Open read-write. Prefer this when the caller needs to journal-replay
    /// or apply Phase 4 mutations. Falls back to an error if the path is
    /// not writable.
    pub fn open_rw(path: &str) -> Result<Self> {
        Self::open_path_rw(Path::new(path))
    }

    /// Open read-write by a path that need not be UTF-8; see
    /// [`FileDevice::open_path`].
    pub fn open_path_rw(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file: Mutex::new(file),
            size,
            writable: true,
        })
    }

    /// Open read-write if possible; otherwise fall back to read-only. Useful
    /// for the mount path so read-only images on e.g. a locked volume still
    /// mount, just without replay.
    pub fn open_best_effort(path: &str) -> Result<Self> {
        match Self::open_rw(path) {
            Ok(d) => Ok(d),
            Err(_) => Self::open(path),
        }
    }
}

impl BlockDevice for FileDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(offset))?;
        f.read_exact(buf)?;
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.size
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if !self.writable {
            return Err(Error::Corrupt("FileDevice opened read-only"));
        }
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(buf)?;
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        if !self.writable {
            return Ok(());
        }
        let mut f = self.file.lock().unwrap();
        f.flush()?;
        f.sync_data()?;
        Ok(())
    }

    fn is_writable(&self) -> bool {
        self.writable
    }
}

/// Read callback: fill `buf` starting at byte `offset`.
pub type ReadCb = Box<dyn Fn(u64, &mut [u8]) -> std::io::Result<()> + Send + Sync>;
/// Write callback: write `buf` starting at byte `offset`.
pub type WriteCb = Box<dyn Fn(u64, &[u8]) -> std::io::Result<()> + Send + Sync>;
/// Flush callback.
pub type FlushCb = Box<dyn Fn() -> std::io::Result<()> + Send + Sync>;

/// Callback-backed device — used when the host process owns the fd
/// (e.g. FSBlockDeviceResource via the C bridge). Optional write callback;
/// set to `None` for read-only.
pub struct CallbackDevice {
    pub size: u64,
    pub read: ReadCb,
    pub write: Option<WriteCb>,
    pub flush: Option<FlushCb>,
}

impl BlockDevice for CallbackDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (self.read)(offset, buf)?;
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.size
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        match &self.write {
            Some(f) => {
                f(offset, buf)?;
                Ok(())
            }
            None => Err(Error::Corrupt("CallbackDevice has no write callback")),
        }
    }

    fn flush(&self) -> Result<()> {
        match &self.flush {
            Some(f) => {
                f()?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn is_writable(&self) -> bool {
        self.write.is_some()
    }
}

// ---------------------------------------------------------------------------
// AlignedDevice — sector-aligned I/O over a device that requires it
// ---------------------------------------------------------------------------

/// Presents a byte-granular [`BlockDevice`] over one that accepts only
/// requests whose offset and length are multiples of `sector` (#373).
///
/// The engine reads 1024 bytes at offset 1024 for the superblock, and its
/// write path passes sub-block writes through; a host block resource that
/// only takes sector-aligned I/O refuses both. Here:
///
/// - an aligned request passes straight through, with no copy;
/// - an unaligned read reads the enclosing aligned span into a bounce
///   buffer and copies the requested slice out;
/// - an unaligned write reads the enclosing span's first and last sectors,
///   patches the caller's bytes in, and writes the whole span back as one
///   aligned request.
///
/// Every write holds one lock, so a read-modify-write can neither lose nor
/// be lost to a concurrent write into the same sector.
pub struct AlignedDevice<D: BlockDevice> {
    inner: D,
    sector: u64,
    write_lock: Mutex<()>,
}

impl<D: BlockDevice> AlignedDevice<D> {
    /// Wrap `inner`, whose requests must be multiples of `sector` bytes.
    /// `sector` must be a power of two.
    pub fn new(inner: D, sector: u32) -> Self {
        assert!(
            sector.is_power_of_two(),
            "sector size {sector} is not a power of two"
        );
        AlignedDevice {
            inner,
            sector: sector as u64,
            write_lock: Mutex::new(()),
        }
    }

    fn is_aligned(&self, offset: u64, len: usize) -> bool {
        let mask = self.sector - 1;
        offset & mask == 0 && (len as u64) & mask == 0
    }

    /// The aligned span `[start, end)` enclosing `len` bytes at `offset`.
    fn span(&self, offset: u64, len: usize) -> Result<(u64, u64)> {
        let mask = self.sector - 1;
        let end = offset
            .checked_add(len as u64)
            .and_then(|e| e.checked_add(mask))
            .ok_or(Error::Corrupt("device request overflows u64"))?
            & !mask;
        if end > self.inner.size_bytes() {
            return Err(Error::Corrupt("device request past the end of the device"));
        }
        Ok((offset & !mask, end))
    }
}

impl<D: BlockDevice> BlockDevice for AlignedDevice<D> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if self.is_aligned(offset, buf.len()) {
            return self.inner.read_at(offset, buf);
        }
        let (start, end) = self.span(offset, buf.len())?;
        let mut bounce = vec![0u8; (end - start) as usize];
        self.inner.read_at(start, &mut bounce)?;
        let head = (offset - start) as usize;
        buf.copy_from_slice(&bounce[head..head + buf.len()]);
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        if self.is_aligned(offset, buf.len()) {
            return self.inner.write_at(offset, buf);
        }
        let (start, end) = self.span(offset, buf.len())?;
        let sector = self.sector as usize;
        let mut bounce = vec![0u8; (end - start) as usize];
        let head = (offset - start) as usize;
        let tail = head + buf.len();
        // Only the first and last sectors hold bytes the caller is not
        // replacing; every sector between them is overwritten whole.
        if head != 0 {
            self.inner.read_at(start, &mut bounce[..sector])?;
        }
        let last = bounce.len() - sector;
        if !tail.is_multiple_of(sector) && (last != 0 || head == 0) {
            self.inner
                .read_at(start + last as u64, &mut bounce[last..])?;
        }
        bounce[head..tail].copy_from_slice(buf);
        self.inner.write_at(start, &bounce)
    }

    fn flush(&self) -> Result<()> {
        self.inner.flush()
    }

    fn is_writable(&self) -> bool {
        self.inner.is_writable()
    }

    fn populate_cache(&self, block: u64, bytes: Vec<u8>) {
        self.inner.populate_cache(block, bytes)
    }

    fn unpin_all(&self) {
        self.inner.unpin_all()
    }

    fn invalidate_cache(&self) -> Result<()> {
        self.inner.invalidate_cache()
    }
}

// ---------------------------------------------------------------------------
// CachingDevice — small LRU block cache decorator
// ---------------------------------------------------------------------------

#[cfg(test)]
mod aligned_device_tests {
    use super::*;

    const SECTOR: u64 = 512;

    /// An in-memory device that refuses any request not aligned to SECTOR.
    struct Strict(Mutex<Vec<u8>>);

    impl Strict {
        fn check(offset: u64, len: usize) -> Result<()> {
            if !offset.is_multiple_of(SECTOR) || !(len as u64).is_multiple_of(SECTOR) {
                return Err(Error::Corrupt("unaligned request"));
            }
            Ok(())
        }
    }

    impl BlockDevice for Strict {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            Self::check(offset, buf.len())?;
            let b = self.0.lock().unwrap();
            buf.copy_from_slice(&b[offset as usize..offset as usize + buf.len()]);
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            self.0.lock().unwrap().len() as u64
        }
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
            Self::check(offset, buf.len())?;
            let mut b = self.0.lock().unwrap();
            b[offset as usize..offset as usize + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn is_writable(&self) -> bool {
            true
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + 3) as u8).collect()
    }

    /// Every (offset, length) shape against a 4-sector device: inside one
    /// sector, touching either edge, straddling one boundary, spanning
    /// whole sectors between two partial ones, and fully aligned.
    #[test]
    fn unaligned_reads_and_writes_match_a_byte_granular_model() {
        let size = 4 * SECTOR as usize;
        let cases: &[(u64, usize)] = &[
            (0, 1),
            (1, 10),
            (500, 12),
            (511, 1),
            (510, 4),
            (0, 512),
            (512, 1024),
            (100, 1500),
            (1, 2046),
            (1024, 1),
            (2047, 1),
            (0, 2048),
            (3, 0),
        ];
        for &(off, len) in cases {
            let base = pattern(size);
            let dev = AlignedDevice::new(Strict(Mutex::new(base.clone())), SECTOR as u32);
            let data: Vec<u8> = (0..len).map(|i| !(i as u8)).collect();
            dev.write_at(off, &data)
                .unwrap_or_else(|e| panic!("write {len}@{off}: {e:?}"));
            let mut model = base;
            model[off as usize..off as usize + len].copy_from_slice(&data);
            assert!(
                *dev.inner.0.lock().unwrap() == model,
                "write {len}@{off} disturbed bytes outside it"
            );
            let mut back = vec![0u8; len];
            dev.read_at(off, &mut back)
                .unwrap_or_else(|e| panic!("read {len}@{off}: {e:?}"));
            assert_eq!(back, data, "read {len}@{off}");
        }
    }

    #[test]
    fn a_request_past_the_end_is_refused_not_wrapped() {
        let dev = AlignedDevice::new(Strict(Mutex::new(vec![0; 1024])), SECTOR as u32);
        let mut buf = [0u8; 4];
        assert!(dev.read_at(1022, &mut buf).is_err());
        assert!(dev.write_at(1022, &buf).is_err());
        assert!(dev.read_at(u64::MAX - 1, &mut buf).is_err());
    }
}
