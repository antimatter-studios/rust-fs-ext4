//! `RO_COMPAT_LARGE_FILE` (0x0002) and the files that need it.
//!
//! The format documentation's superblock table describes the flag as "this
//! filesystem has been used to store a file greater than 2 GiB": a regular
//! file whose `i_size` does not fit in 31 bits. `e2fsck` refuses a volume
//! holding such a file without it. Two things have to hold here:
//!
//! - A fresh volume carries the flag, as `mke2fs` makes one by default
//!   (`tests/large_file_oracle.rs` compares against `mke2fs` itself).
//! - A volume without it gains it the moment a write grows a regular file
//!   past 2 GiB - 1, in the same transaction as the size, and not before.
//! - Without a journal, the flag reaches the disk before the size does, so
//!   a crash at any write of that commit never leaves the file without it.
//!
//! In-memory volume: no fixture, no VM.

use fs_ext4::block_io::BlockDevice;
use fs_ext4::error::Result;
use fs_ext4::features::{FsFlavor, RoCompat};
use fs_ext4::{mkfs, Filesystem};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The largest size a file may have on a volume without the flag.
const SMALL_FILE_MAX: u64 = (1 << 31) - 1;

const NEVER: usize = usize::MAX;

/// An in-memory device that can be made to lose power: once armed, the
/// `crash_at`-th write (counting from zero) and every write after it fail
/// and change nothing, as if the machine stopped there.
struct MemDev {
    bytes: Mutex<Vec<u8>>,
    seen: AtomicUsize,
    crash_at: AtomicUsize,
}

impl MemDev {
    fn arm(&self, crash_at: usize) {
        self.seen.store(0, Ordering::SeqCst);
        self.crash_at.store(crash_at, Ordering::SeqCst);
    }

    /// Writes asked for since [`Self::arm`], the failed ones included.
    fn writes_seen(&self) -> usize {
        self.seen.load(Ordering::SeqCst)
    }
}

impl BlockDevice for MemDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let b = self.bytes.lock().unwrap();
        let start = offset as usize;
        buf.copy_from_slice(&b[start..start + buf.len()]);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.bytes.lock().unwrap().len() as u64
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.seen.fetch_add(1, Ordering::SeqCst) >= self.crash_at.load(Ordering::SeqCst) {
            return Err(fs_ext4::Error::Corrupt("injected power loss"));
        }
        let mut b = self.bytes.lock().unwrap();
        let start = offset as usize;
        b[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

fn formatted(flavor: FsFlavor, block_size: u32) -> Arc<MemDev> {
    let size: u64 = 32 << 20;
    let dev = Arc::new(MemDev {
        bytes: Mutex::new(vec![0u8; size as usize]),
        seen: AtomicUsize::new(0),
        crash_at: AtomicUsize::new(NEVER),
    });
    mkfs::format_filesystem_with_flavor(
        dev.as_ref(),
        None,
        Some([3; 16]),
        size,
        block_size,
        flavor,
    )
    .expect("format");
    dev
}

/// Take `LARGE_FILE` off the superblock, as a volume made without it has,
/// keeping the superblock checksum right on a metadata_csum volume.
fn clear_large_file(dev: &MemDev) {
    let mut b = dev.bytes.lock().unwrap();
    let sb = &mut b[1024..2048];
    let ro = u32::from_le_bytes(sb[0x64..0x68].try_into().unwrap());
    sb[0x64..0x68].copy_from_slice(&(ro & !RoCompat::LARGE_FILE.bits()).to_le_bytes());
    if ro & RoCompat::METADATA_CSUM.bits() != 0 {
        let c = fs_ext4::checksum::linux_crc32c(!0, &sb[..0x3FC]);
        sb[0x3FC..0x400].copy_from_slice(&c.to_le_bytes());
    }
}

fn has_large_file(dev: &Arc<MemDev>) -> bool {
    let fs = Filesystem::mount(dev.clone()).expect("remount");
    fs.sb.feature_ro_compat & RoCompat::LARGE_FILE.bits() != 0
}

/// A volume without the flag, holding an empty regular file `/f`, and
/// that file's inode number.
fn without_flag(flavor: FsFlavor, block_size: u32) -> (Arc<MemDev>, u32) {
    let dev = formatted(flavor, block_size);
    clear_large_file(&dev);
    assert!(!has_large_file(&dev), "the flag was cleared for the test");
    let fs = Filesystem::mount(dev.clone()).expect("mount");
    let ino = fs.apply_create("/f", 0o644).expect("create /f");
    (dev, ino)
}

#[test]
fn every_flavor_is_formatted_with_large_file() {
    for (flavor, bs) in [
        (FsFlavor::Ext2, 4096),
        (FsFlavor::Ext3, 4096),
        (FsFlavor::Ext4, 2048),
        (FsFlavor::Ext4, 4096),
    ] {
        assert!(
            has_large_file(&formatted(flavor, bs)),
            "{flavor:?} at {bs}-byte blocks: a fresh volume carries large_file, \
             as mke2fs makes one"
        );
    }
}

#[test]
fn a_write_that_ends_past_2_gib_sets_the_flag() {
    let (dev, _) = without_flag(FsFlavor::Ext4, 4096);
    {
        let fs = Filesystem::mount(dev.clone()).expect("mount");
        let size = fs.apply_pwrite("/f", SMALL_FILE_MAX, b"x").expect("pwrite");
        assert_eq!(size, SMALL_FILE_MAX + 1);
    }
    assert!(
        has_large_file(&dev),
        "a regular file of 2 GiB needs large_file"
    );
}

#[test]
fn a_truncate_past_2_gib_sets_the_flag() {
    for flavor in [FsFlavor::Ext4, FsFlavor::Ext2] {
        let (dev, ino) = without_flag(flavor, 4096);
        {
            let fs = Filesystem::mount(dev.clone()).expect("mount");
            fs.apply_truncate_ino(ino, 3 << 30).expect("truncate up");
        }
        assert!(
            has_large_file(&dev),
            "{flavor:?}: a regular file of 3 GiB needs large_file"
        );
    }
}

#[test]
fn a_file_of_2_gib_minus_1_leaves_the_flag_alone() {
    let (dev, ino) = without_flag(FsFlavor::Ext4, 4096);
    {
        let fs = Filesystem::mount(dev.clone()).expect("mount");
        fs.apply_truncate_ino(ino, SMALL_FILE_MAX)
            .expect("truncate up");
        fs.apply_pwrite("/f", SMALL_FILE_MAX - 1, b"x")
            .expect("pwrite");
    }
    assert!(
        !has_large_file(&dev),
        "2 GiB - 1 fits in 31 bits: the flag is not needed, and is not set"
    );
}

/// Without a journal a commit is a series of plain writes. Wherever power
/// is lost among them, the disk must not hold a file past 2 GiB - 1 on a
/// volume without the flag: the flag has to reach the disk first.
#[test]
fn without_a_journal_the_flag_reaches_the_disk_before_the_size() {
    let mut crash_at = 0;
    loop {
        let (dev, ino) = without_flag(FsFlavor::Ext4, 4096);
        {
            let fs = Filesystem::mount(dev.clone()).expect("mount");
            assert!(fs.journal.is_none(), "the default format has no journal");
            dev.arm(crash_at);
            let _ = fs.apply_truncate_ino(ino, 3 << 30);
        }
        let completed = dev.writes_seen() <= crash_at;
        dev.arm(NEVER);

        let fs = Filesystem::mount(dev.clone()).expect("mount after the crash");
        let size = fs.read_inode_verified(ino).expect("inode").0.size;
        let flag = fs.sb.feature_ro_compat & RoCompat::LARGE_FILE.bits() != 0;
        assert!(
            size <= SMALL_FILE_MAX || flag,
            "power lost at write {crash_at}: /f is {size} bytes on a volume \
             without large_file"
        );
        if completed {
            assert_eq!(size, 3 << 30, "the uninterrupted truncate completes");
            assert!(crash_at >= 2, "a commit of at least two writes was tried");
            break;
        }
        crash_at += 1;
    }
}
