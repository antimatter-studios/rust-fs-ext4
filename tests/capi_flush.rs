//! `fs_ext4_flush` and `fs_ext4_fresh_read`: the durability barrier and the
//! cache drop, reached through the C ABI without releasing the mount (#374).
//!
//! A host embedding the engine through C had no way to ask "is everything
//! so far on the device?" short of unmounting, so it unmounted after every
//! mutation and remounted lazily. `Filesystem::flush` and
//! `Filesystem::fresh_read` existed in Rust; C could reach neither.

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::capi::*;
use fs_ext4::Filesystem;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::raw::{c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const IMAGE: &str = "ext4-basic.img";
const EINVAL: c_int = 22;
const EIO: c_int = 5;

struct DevCtx {
    path: PathBuf,
    file: Mutex<File>,
    size: u64,
    flushes: AtomicU64,
    /// When set, the next write fails and clears it.
    fail_next_write: AtomicBool,
}

extern "C" fn read_cb(ctx: *mut c_void, buf: *mut c_void, offset: u64, length: u64) -> c_int {
    let dev = unsafe { &*(ctx as *const DevCtx) };
    if offset.checked_add(length).is_none_or(|end| end > dev.size) {
        return 1;
    }
    let Ok(length) = usize::try_from(length) else {
        return 1;
    };
    let mut file = dev.file.lock().unwrap();
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return 1;
    }
    let bytes = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, length) };
    if file.read_exact(bytes).is_err() {
        return 1;
    }
    0
}

extern "C" fn write_cb(ctx: *mut c_void, buf: *const c_void, offset: u64, length: u64) -> c_int {
    let dev = unsafe { &*(ctx as *const DevCtx) };
    if dev.fail_next_write.swap(false, Ordering::SeqCst) {
        return 1;
    }
    if offset.checked_add(length).is_none_or(|end| end > dev.size) {
        return 1;
    }
    let Ok(length) = usize::try_from(length) else {
        return 1;
    };
    let mut file = dev.file.lock().unwrap();
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return 1;
    }
    let bytes = unsafe { std::slice::from_raw_parts(buf as *const u8, length) };
    if file.write_all(bytes).is_err() {
        return 1;
    }
    0
}

extern "C" fn flush_cb(ctx: *mut c_void) -> c_int {
    let dev = unsafe { &*(ctx as *const DevCtx) };
    if dev.file.lock().unwrap().flush().is_err() {
        return 1;
    }
    dev.flushes.fetch_add(1, Ordering::SeqCst);
    0
}

fn dev_from(path: &Path) -> Arc<DevCtx> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open callback image");
    let size = file.metadata().expect("callback image metadata").len();
    Arc::new(DevCtx {
        path: path.to_path_buf(),
        file: Mutex::new(file),
        size,
        flushes: AtomicU64::new(0),
        fail_next_write: AtomicBool::new(false),
    })
}

fn fixture_device() -> Arc<DevCtx> {
    let path = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), IMAGE);
    let copy = fs_ext4_test_support::snapshot_file(&path)
        .unwrap_or_else(|e| panic!("copy fixture {path}: {e}"));
    dev_from(&copy)
}

fn cfg(dev: &Arc<DevCtx>) -> fs_ext4_blockdev_cfg_t {
    fs_ext4_blockdev_cfg_t {
        read: Some(read_cb),
        context: Arc::as_ptr(dev) as *mut c_void,
        size_bytes: dev.size,
        block_size: 512,
        write: Some(write_cb),
        flush: Some(flush_cb),
    }
}

fn mount_rw(dev: &Arc<DevCtx>) -> *mut fs_ext4_fs_t {
    let c = cfg(dev);
    let fs = unsafe { fs_ext4_mount_rw_with_callbacks(&c) };
    assert!(!fs.is_null(), "rw mount: {}", last_error());
    fs
}

fn last_error() -> String {
    unsafe { std::ffi::CStr::from_ptr(fs_ext4_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn create_and_write(fs: *mut fs_ext4_fs_t, path: &str, data: &[u8]) -> Result<(), c_int> {
    let p = CString::new(path).unwrap();
    if unsafe { fs_ext4_create(fs, p.as_ptr(), 0o644) } == 0 {
        return Err(fs_ext4_last_errno());
    }
    let n = unsafe {
        fs_ext4_pwrite(
            fs,
            p.as_ptr(),
            data.as_ptr() as *const c_void,
            data.len() as u64,
            0,
        )
    };
    if n != data.len() as i64 {
        return Err(fs_ext4_last_errno());
    }
    Ok(())
}

fn read_whole(fs: *mut fs_ext4_fs_t, path: &str, len: usize) -> Option<Vec<u8>> {
    let p = CString::new(path).unwrap();
    let mut buf = vec![0u8; len + 16];
    let n = unsafe {
        fs_ext4_read_file(
            fs,
            p.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            0,
            buf.len() as u64,
        )
    };
    if n < 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(buf)
}

fn payload() -> Vec<u8> {
    (0..10_000u32).map(|i| (i * 31 % 251) as u8).collect()
}

/// After `fs_ext4_flush` returns 0, the device alone -- copied at that
/// moment, with the first mount still live -- holds the file byte-exact.
#[test]
fn flush_puts_every_change_on_the_device_without_unmounting() {
    let dev = fixture_device();
    let fs = mount_rw(&dev);
    let data = payload();
    create_and_write(fs, "/flushed.bin", &data).expect("create + pwrite");

    let before = dev.flushes.load(Ordering::SeqCst);
    let rc = unsafe { fs_ext4_flush(fs) };
    assert_eq!(rc, 0, "fs_ext4_flush: {}", last_error());
    assert_eq!(fs_ext4_last_errno(), 0);
    assert!(
        dev.flushes.load(Ordering::SeqCst) > before,
        "fs_ext4_flush did not reach the host's flush callback"
    );

    // A snapshot of the device now, read by an independent read-only mount.
    let snapshot = fs_ext4_test_support::snapshot_file(&dev.path).expect("snapshot device");
    let copy = dev_from(&snapshot);
    let c = cfg(&copy);
    let ro = unsafe { fs_ext4_mount_with_callbacks(&c) };
    assert!(!ro.is_null(), "ro mount of the copy: {}", last_error());
    assert_eq!(
        read_whole(ro, "/flushed.bin", data.len()).as_deref(),
        Some(&data[..]),
        "the device copy taken after flush does not hold the file"
    );
    unsafe { fs_ext4_umount(ro) };

    // The mount was not released: it keeps working.
    create_and_write(fs, "/after.bin", b"still mounted").expect("write after flush");
    assert_eq!(unsafe { fs_ext4_flush(fs) }, 0, "{}", last_error());
    unsafe { fs_ext4_umount(fs) };
}

/// A mutation whose device write failed leaves the mount unfit to vouch for
/// anything: flush refuses with EIO rather than claim durability.
#[test]
fn flush_after_a_failed_write_is_eio() {
    let dev = fixture_device();
    let fs = mount_rw(&dev);
    create_and_write(fs, "/first.bin", b"first").expect("first write");
    assert_eq!(unsafe { fs_ext4_flush(fs) }, 0, "{}", last_error());

    dev.fail_next_write.store(true, Ordering::SeqCst);
    assert!(
        create_and_write(fs, "/second.bin", &payload()).is_err(),
        "the mutation succeeded although its device write failed"
    );
    assert_eq!(
        unsafe { fs_ext4_flush(fs) },
        -1,
        "flush after a failed write"
    );
    assert_eq!(fs_ext4_last_errno(), EIO, "{}", last_error());
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn null_handles_are_einval() {
    assert_eq!(unsafe { fs_ext4_flush(std::ptr::null_mut()) }, -1);
    assert_eq!(fs_ext4_last_errno(), EINVAL);
    assert_eq!(unsafe { fs_ext4_fresh_read(std::ptr::null_mut()) }, -1);
    assert_eq!(fs_ext4_last_errno(), EINVAL);
}

/// A read-only mount has nothing to write back: flush succeeds.
#[test]
fn flush_on_a_read_only_mount_succeeds() {
    let dev = fixture_device();
    let c = cfg(&dev);
    let ro = unsafe { fs_ext4_mount_with_callbacks(&c) };
    assert!(!ro.is_null(), "{}", last_error());
    assert_eq!(unsafe { fs_ext4_flush(ro) }, 0, "{}", last_error());
    unsafe { fs_ext4_umount(ro) };
}

/// The host changed the device underneath an idle mount (here: a second
/// mount wrote a file and released). `fs_ext4_fresh_read` drops the first
/// mount's caches, so it reads what the device now holds.
#[test]
fn fresh_read_sees_what_the_host_wrote_underneath() {
    let dev = fixture_device();
    let first = mount_rw(&dev);
    // Warm the first mount's caches: the lookup reads the root directory.
    let name = CString::new("/underneath.bin").unwrap();
    let mut attr = unsafe { std::mem::zeroed::<fs_ext4_attr_t>() };
    assert_eq!(unsafe { fs_ext4_stat(first, name.as_ptr(), &mut attr) }, -1);
    assert_eq!(unsafe { fs_ext4_flush(first) }, 0, "{}", last_error());

    let data = payload();
    let second = mount_rw(&dev);
    create_and_write(second, "/underneath.bin", &data).expect("second mount write");
    unsafe { fs_ext4_umount(second) };

    let rc = unsafe { fs_ext4_fresh_read(first) };
    assert_eq!(rc, 0, "fs_ext4_fresh_read: {}", last_error());
    assert_eq!(
        read_whole(first, "/underneath.bin", data.len()).as_deref(),
        Some(&data[..]),
        "after fresh_read the mount does not see the device's current state"
    );
    unsafe { fs_ext4_umount(first) };
}

/// Keeps writes only until the second flush after it is armed: a commit's
/// journal blocks and its dirty journal superblock. Then the power goes.
struct CutAfterDirtyJournal {
    inner: Arc<dyn BlockDevice>,
    armed: AtomicBool,
    flushes: AtomicUsize,
}

impl BlockDevice for CutAfterDirtyJournal {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_ext4::error::Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_ext4::error::Result<()> {
        if self.armed.load(Ordering::SeqCst) && self.flushes.load(Ordering::SeqCst) >= 2 {
            return Ok(());
        }
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> fs_ext4::error::Result<()> {
        if self.armed.load(Ordering::SeqCst) {
            self.flushes.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.flush()
    }
    fn is_writable(&self) -> bool {
        true
    }
}

/// A read-only mount of a dirty journal replays it into its cache only: the
/// device still holds the pre-replay bytes. `fs_ext4_fresh_read` refuses
/// with EIO rather than discard the replayed view, and the mount keeps it.
#[test]
fn fresh_read_on_a_read_only_replay_is_eio_and_keeps_the_replayed_view() {
    let image = fs_ext4_test_support::temp_path!("fs_ext4_capi_flush_{}.img", std::process::id());
    fs_ext4_test_support::copy_file_sparse(
        fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), IMAGE),
        &image,
    )
    .unwrap();
    {
        let dev = Arc::new(CutAfterDirtyJournal {
            inner: Arc::new(FileDevice::open_rw(&image).unwrap()),
            armed: AtomicBool::new(false),
            flushes: AtomicUsize::new(0),
        });
        let fs = Filesystem::mount(dev.clone()).expect("mount rw");
        assert!(fs.journal.is_some(), "the fixture must be journaled");
        fs.apply_mkdir("/warmup", 0o755).expect("warm-up mkdir");
        dev.armed.store(true, Ordering::SeqCst);
        fs.apply_mkdir("/committed", 0o755).expect("mkdir");
    }

    let path = CString::new(image.as_str()).unwrap();
    let committed = CString::new("/committed").unwrap();
    let mut attr = unsafe { std::mem::zeroed::<fs_ext4_attr_t>() };
    let ro = unsafe { fs_ext4_mount(path.as_ptr()) };
    assert!(!ro.is_null(), "{}", last_error());
    assert_eq!(
        unsafe { fs_ext4_stat(ro, committed.as_ptr(), &mut attr) },
        0,
        "the read-only mount did not replay the committed mkdir"
    );

    assert_eq!(unsafe { fs_ext4_fresh_read(ro) }, -1);
    assert_eq!(fs_ext4_last_errno(), EIO, "{}", last_error());
    assert_eq!(
        unsafe { fs_ext4_stat(ro, committed.as_ptr(), &mut attr) },
        0,
        "the refused fresh_read discarded the replayed view"
    );
    unsafe { fs_ext4_umount(ro) };
    let _ = std::fs::remove_file(&image);
}
