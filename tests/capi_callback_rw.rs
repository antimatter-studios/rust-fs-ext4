//! End-to-end tests for `fs_ext4_mount_rw_with_callbacks`.
//!
//! Validates the v0.1.3 RW callback path: the FSKit-style consumer owns
//! the device bytes (here, an `Arc<Mutex<Vec<u8>>>`) and surfaces them
//! to the driver through C function pointers.
//!
//! The driver should be able to create / read / write / unlink files
//! against that backing buffer, and the same buffer should reflect the
//! mutations after unmount (i.e. the writes really go through the
//! callback, not just into a private cache).

use fs_ext4::capi::*;
use std::ffi::CString;
use std::os::raw::{c_int, c_void};
use std::sync::{Arc, Mutex};

const IMAGE: &str = "ext4-basic.img";

// ---------------------------------------------------------------------------
// Mutex<Vec<u8>>-backed device with a dirty-flag we can probe from tests.
// ---------------------------------------------------------------------------

struct DevCtx {
    /// Raw device bytes.
    bytes: Mutex<Vec<u8>>,
    /// Number of times the write callback has been invoked.
    writes: Mutex<u64>,
    /// Number of times the flush callback has been invoked.
    flushes: Mutex<u64>,
}

extern "C" fn read_cb(ctx: *mut c_void, buf: *mut c_void, offset: u64, length: u64) -> c_int {
    if ctx.is_null() || buf.is_null() {
        return 1;
    }
    let dev = unsafe { &*(ctx as *const DevCtx) };
    let bytes = dev.bytes.lock().unwrap();
    let end = (offset as usize).checked_add(length as usize);
    let Some(end) = end else { return 2 };
    if end > bytes.len() {
        return 3;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr().add(offset as usize),
            buf as *mut u8,
            length as usize,
        );
    }
    0
}

extern "C" fn write_cb(ctx: *mut c_void, buf: *const c_void, offset: u64, length: u64) -> c_int {
    if ctx.is_null() || buf.is_null() {
        return 1;
    }
    let dev = unsafe { &*(ctx as *const DevCtx) };
    let mut bytes = dev.bytes.lock().unwrap();
    let end = (offset as usize).checked_add(length as usize);
    let Some(end) = end else { return 2 };
    if end > bytes.len() {
        return 3;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            buf as *const u8,
            bytes.as_mut_ptr().add(offset as usize),
            length as usize,
        );
    }
    *dev.writes.lock().unwrap() += 1;
    0
}

extern "C" fn flush_cb(ctx: *mut c_void) -> c_int {
    if ctx.is_null() {
        return 1;
    }
    let dev = unsafe { &*(ctx as *const DevCtx) };
    *dev.flushes.lock().unwrap() += 1;
    0
}

fn fresh_dev() -> Arc<DevCtx> {
    let path = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), IMAGE);
    let bytes = fs_ext4_test_support::read_small_file(&path)
        .unwrap_or_else(|e| panic!("read fixture image {path}: {e}"));
    Arc::new(DevCtx {
        bytes: Mutex::new(bytes),
        writes: Mutex::new(0),
        flushes: Mutex::new(0),
    })
}

fn make_cfg(dev: &Arc<DevCtx>, with_write: bool, with_flush: bool) -> fs_ext4_blockdev_cfg_t {
    let size = dev.bytes.lock().unwrap().len() as u64;
    fs_ext4_blockdev_cfg_t {
        read: Some(read_cb),
        context: Arc::as_ptr(dev) as *mut c_void,
        size_bytes: size,
        block_size: 512,
        write: if with_write { Some(write_cb) } else { None },
        flush: if with_flush { Some(flush_cb) } else { None },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn rw_callback_create_write_stat_read_unlink_round_trip() {
    let dev = fresh_dev();
    let cfg = make_cfg(&dev, true, true);

    let fs_h = unsafe { fs_ext4_mount_rw_with_callbacks(&cfg) };
    assert!(!fs_h.is_null(), "rw callback mount failed");

    let path = CString::new("/foo.txt").unwrap();
    let payload: &[u8] = b"hello";

    // create -> nonzero inode
    let ino = unsafe { fs_ext4_create(fs_h, path.as_ptr(), 0o644) };
    assert_ne!(ino, 0, "create returned 0");

    // write content
    let n = unsafe {
        fs_ext4_write_file(
            fs_h,
            path.as_ptr(),
            payload.as_ptr() as *const c_void,
            payload.len() as u64,
        )
    };
    assert_eq!(n, payload.len() as i64, "write_file size mismatch");

    // stat -> size matches
    let mut attr = unsafe { std::mem::zeroed::<fs_ext4_attr_t>() };
    let rc = unsafe { fs_ext4_stat(fs_h, path.as_ptr(), &mut attr) };
    assert_eq!(rc, 0, "stat after write failed");
    assert_eq!(attr.size, payload.len() as u64);

    // read -> bytes match
    let mut buf = [0u8; 16];
    let read_len = unsafe {
        fs_ext4_read_file(
            fs_h,
            path.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            0,
            buf.len() as u64,
        )
    };
    assert_eq!(read_len, payload.len() as i64);
    assert_eq!(&buf[..payload.len()], payload);

    // unlink -> stat fails
    let rc = unsafe { fs_ext4_unlink(fs_h, path.as_ptr()) };
    assert_eq!(rc, 0, "unlink failed");
    let mut attr = unsafe { std::mem::zeroed::<fs_ext4_attr_t>() };
    let rc = unsafe { fs_ext4_stat(fs_h, path.as_ptr(), &mut attr) };
    assert_ne!(rc, 0, "stat must fail after unlink");

    unsafe { fs_ext4_umount(fs_h) };

    // The write callback must have been invoked at least once.
    let writes = *dev.writes.lock().unwrap();
    assert!(writes > 0, "write callback was never called");
}

#[test]
fn rw_callback_mkdir_rmdir_chmod_rename() {
    let dev = fresh_dev();
    let cfg = make_cfg(&dev, true, false);

    let fs_h = unsafe { fs_ext4_mount_rw_with_callbacks(&cfg) };
    assert!(!fs_h.is_null());

    // mkdir /newdir
    let dir = CString::new("/newdir").unwrap();
    let dir_ino = unsafe { fs_ext4_mkdir(fs_h, dir.as_ptr(), 0o755) };
    assert_ne!(dir_ino, 0, "mkdir failed");

    // create /newdir/a.txt
    let a = CString::new("/newdir/a.txt").unwrap();
    assert_ne!(unsafe { fs_ext4_create(fs_h, a.as_ptr(), 0o600) }, 0);

    // rename /newdir/a.txt -> /newdir/b.txt
    let b = CString::new("/newdir/b.txt").unwrap();
    let rc = unsafe { fs_ext4_rename(fs_h, a.as_ptr(), b.as_ptr()) };
    assert_eq!(rc, 0, "rename failed");

    // chmod /newdir/b.txt 0o644
    let rc = unsafe { fs_ext4_chmod(fs_h, b.as_ptr(), 0o644) };
    assert_eq!(rc, 0, "chmod failed");

    let mut attr = unsafe { std::mem::zeroed::<fs_ext4_attr_t>() };
    assert_eq!(unsafe { fs_ext4_stat(fs_h, b.as_ptr(), &mut attr) }, 0);
    assert_eq!(attr.mode & 0o777, 0o644);

    // unlink + rmdir
    assert_eq!(unsafe { fs_ext4_unlink(fs_h, b.as_ptr()) }, 0);
    assert_eq!(unsafe { fs_ext4_rmdir(fs_h, dir.as_ptr()) }, 0);

    unsafe { fs_ext4_umount(fs_h) };
}

#[test]
fn rw_callback_null_write_callback_returns_einval() {
    let dev = fresh_dev();
    // read present, write missing — must reject.
    let cfg = make_cfg(&dev, false, false);
    let fs_h = unsafe { fs_ext4_mount_rw_with_callbacks(&cfg) };
    assert!(fs_h.is_null(), "RW callback mount must require write fn");
    assert_eq!(fs_ext4_last_errno(), 22, "EINVAL expected");
}

#[test]
fn rw_callback_null_read_callback_returns_einval() {
    let dev = fresh_dev();
    // Manually build a cfg with read=None, write=Some — still EINVAL because
    // read is required even on RW mounts.
    let size = dev.bytes.lock().unwrap().len() as u64;
    let cfg = fs_ext4_blockdev_cfg_t {
        read: None,
        context: Arc::as_ptr(&dev) as *mut c_void,
        size_bytes: size,
        block_size: 512,
        write: Some(write_cb),
        flush: None,
    };
    let fs_h = unsafe { fs_ext4_mount_rw_with_callbacks(&cfg) };
    assert!(fs_h.is_null());
    assert_eq!(fs_ext4_last_errno(), 22, "EINVAL expected");
}

#[test]
fn rw_callback_null_cfg_returns_einval() {
    let fs_h = unsafe { fs_ext4_mount_rw_with_callbacks(std::ptr::null()) };
    assert!(fs_h.is_null());
    assert_eq!(fs_ext4_last_errno(), 22, "EINVAL expected");
}

#[test]
fn ro_callback_mount_unchanged_still_rejects_writes() {
    // The OLD `fs_ext4_mount_with_callbacks` is a documented RO entry point.
    // It must still mount RO even if the cfg has a write callback — and it
    // must still refuse mutating ops.
    let dev = fresh_dev();
    let cfg = make_cfg(&dev, true, true); // write+flush set, but RO call should ignore them.

    let fs_h = unsafe { fs_ext4_mount_with_callbacks(&cfg) };
    assert!(!fs_h.is_null(), "RO callback mount failed");

    let path = CString::new("/should_fail.txt").unwrap();
    let ino = unsafe { fs_ext4_create(fs_h, path.as_ptr(), 0o644) };
    assert_eq!(ino, 0, "create on RO callback mount must fail");
    assert_ne!(fs_ext4_last_errno(), 0, "errno must be set on failure");

    unsafe { fs_ext4_umount(fs_h) };

    // No writes should have happened on the underlying device.
    let writes = *dev.writes.lock().unwrap();
    assert_eq!(writes, 0, "RO mount must not invoke write callback");
}

#[test]
fn rw_callback_writes_are_persisted_across_remount() {
    // Mutate via RW callback → unmount → re-mount RO and verify the change
    // survives. Confirms the write callback's bytes really land in the
    // backing buffer (not just an in-memory cache).
    let dev = fresh_dev();
    let path = CString::new("/persist.txt").unwrap();
    let payload: &[u8] = b"persist me";

    {
        let cfg = make_cfg(&dev, true, false);
        let fs_h = unsafe { fs_ext4_mount_rw_with_callbacks(&cfg) };
        assert!(!fs_h.is_null());
        assert_ne!(unsafe { fs_ext4_create(fs_h, path.as_ptr(), 0o644) }, 0);
        let n = unsafe {
            fs_ext4_write_file(
                fs_h,
                path.as_ptr(),
                payload.as_ptr() as *const c_void,
                payload.len() as u64,
            )
        };
        assert_eq!(n, payload.len() as i64);
        unsafe { fs_ext4_umount(fs_h) };
    }

    {
        // Re-mount the SAME backing buffer as RO and verify the file is there.
        let cfg = make_cfg(&dev, false, false);
        let fs_h = unsafe { fs_ext4_mount_with_callbacks(&cfg) };
        assert!(!fs_h.is_null(), "RO remount of mutated bytes failed");
        let mut buf = [0u8; 32];
        let read_len = unsafe {
            fs_ext4_read_file(
                fs_h,
                path.as_ptr(),
                buf.as_mut_ptr() as *mut c_void,
                0,
                buf.len() as u64,
            )
        };
        assert_eq!(read_len, payload.len() as i64);
        assert_eq!(&buf[..payload.len()], payload);
        unsafe { fs_ext4_umount(fs_h) };
    }
}
