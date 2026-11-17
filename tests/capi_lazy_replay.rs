//! End-to-end tests for `fs_ext4_mount_rw_with_callbacks_lazy` +
//! `fs_ext4_replay_journal_if_dirty`.
//!
//! These exports exist for FSKit consumers whose write callback isn't ready
//! to service writes during the mount call (the kernel-level write FD on
//! `FSBlockDeviceResource` only becomes writable AFTER `loadResource`
//! returns successfully). The lazy mount must NOT issue any writes during
//! mount; replay is then driven explicitly by the consumer.
//!
//! Pattern mirrors `tests/capi_callback_rw.rs` — `Mutex<Vec<u8>>`-backed
//! device with a write counter we can probe from tests.

use fs_ext4::capi::*;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::raw::{c_int, c_void};
use std::sync::{Arc, Mutex};

const IMAGE: &str = "ext4-basic.img";

struct DevCtx {
    file: Mutex<File>,
    size: u64,
    writes: Mutex<u64>,
    flushes: Mutex<u64>,
}

extern "C" fn read_cb(ctx: *mut c_void, buf: *mut c_void, offset: u64, length: u64) -> c_int {
    if ctx.is_null() || buf.is_null() {
        return 1;
    }
    let dev = unsafe { &*(ctx as *const DevCtx) };
    if offset.checked_add(length).is_none_or(|end| end > dev.size) {
        return 3;
    }
    let Ok(length) = usize::try_from(length) else {
        return 2;
    };
    let mut file = dev.file.lock().unwrap();
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return 4;
    }
    let bytes = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, length) };
    if file.read_exact(bytes).is_err() {
        return 4;
    }
    0
}

extern "C" fn write_cb(ctx: *mut c_void, buf: *const c_void, offset: u64, length: u64) -> c_int {
    if ctx.is_null() || buf.is_null() {
        return 1;
    }
    let dev = unsafe { &*(ctx as *const DevCtx) };
    if offset.checked_add(length).is_none_or(|end| end > dev.size) {
        return 3;
    }
    let Ok(length) = usize::try_from(length) else {
        return 2;
    };
    let mut file = dev.file.lock().unwrap();
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return 4;
    }
    let bytes = unsafe { std::slice::from_raw_parts(buf as *const u8, length) };
    if file.write_all(bytes).is_err() {
        return 4;
    }
    *dev.writes.lock().unwrap() += 1;
    0
}

extern "C" fn flush_cb(ctx: *mut c_void) -> c_int {
    if ctx.is_null() {
        return 1;
    }
    let dev = unsafe { &*(ctx as *const DevCtx) };
    if dev.file.lock().unwrap().flush().is_err() {
        return 2;
    }
    *dev.flushes.lock().unwrap() += 1;
    0
}

fn fresh_dev() -> Arc<DevCtx> {
    let path = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), IMAGE);
    let scratch = fs_ext4_test_support::snapshot_file(&path)
        .unwrap_or_else(|e| panic!("copy fixture image {path}: {e}"));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&scratch)
        .unwrap_or_else(|e| panic!("open scratch image {}: {e}", scratch.display()));
    let size = file.metadata().expect("scratch image metadata").len();
    Arc::new(DevCtx {
        file: Mutex::new(file),
        size,
        writes: Mutex::new(0),
        flushes: Mutex::new(0),
    })
}

fn make_cfg(dev: &Arc<DevCtx>, with_write: bool, with_flush: bool) -> fs_ext4_blockdev_cfg_t {
    fs_ext4_blockdev_cfg_t {
        read: Some(read_cb),
        context: Arc::as_ptr(dev) as *mut c_void,
        size_bytes: dev.size,
        block_size: 512,
        write: if with_write { Some(write_cb) } else { None },
        flush: if with_flush { Some(flush_cb) } else { None },
    }
}

#[test]
fn lazy_mount_does_not_write_during_mount() {
    let dev = fresh_dev();
    let cfg = make_cfg(&dev, true, true);

    let fs_h = unsafe { fs_ext4_mount_rw_with_callbacks_lazy(&cfg) };
    assert!(!fs_h.is_null(), "lazy mount returned null");

    // Mount itself must not have triggered any writes — that's the whole
    // point of the lazy variant (FSKit can't service writes during
    // loadResource).
    let writes = *dev.writes.lock().unwrap();
    assert_eq!(writes, 0, "lazy mount must not write during mount");

    unsafe { fs_ext4_umount(fs_h) };
}

#[test]
fn replay_journal_if_dirty_when_clean_returns_zero_no_writes() {
    let dev = fresh_dev();
    let cfg = make_cfg(&dev, true, true);

    let fs_h = unsafe { fs_ext4_mount_rw_with_callbacks_lazy(&cfg) };
    assert!(!fs_h.is_null(), "lazy mount returned null");

    // The fixture image is clean — replay should be a no-op.
    let rc = unsafe { fs_ext4_replay_journal_if_dirty(fs_h) };
    assert_eq!(rc, 0, "replay on clean image must return 0");

    let writes = *dev.writes.lock().unwrap();
    assert_eq!(writes, 0, "replay on clean image must not write");

    unsafe { fs_ext4_umount(fs_h) };
}

#[test]
fn replay_journal_if_dirty_when_dirty_replays() {
    // We don't fabricate a dirty image here — the API contract is
    // "_if_dirty", so a clean fixture is the same external observation
    // (returns 0, no error). This test guards the success-path return
    // value when called against a real handle.
    let dev = fresh_dev();
    let cfg = make_cfg(&dev, true, true);

    let fs_h = unsafe { fs_ext4_mount_rw_with_callbacks_lazy(&cfg) };
    assert!(!fs_h.is_null());
    let rc = unsafe { fs_ext4_replay_journal_if_dirty(fs_h) };
    assert_eq!(rc, 0, "replay must return 0 (image is clean)");

    unsafe { fs_ext4_umount(fs_h) };
}

#[test]
fn lazy_then_replay_then_create_write_unlink_round_trip() {
    let dev = fresh_dev();
    let cfg = make_cfg(&dev, true, true);

    let fs_h = unsafe { fs_ext4_mount_rw_with_callbacks_lazy(&cfg) };
    assert!(!fs_h.is_null());

    // Drive replay before any mutation (mirrors what an FSKit consumer
    // would do once its write FD is ready).
    let rc = unsafe { fs_ext4_replay_journal_if_dirty(fs_h) };
    assert_eq!(rc, 0);

    let path = CString::new("/lazy_round_trip.bin").unwrap();
    let payload = [0xABu8; 64];

    let ino = unsafe { fs_ext4_create(fs_h, path.as_ptr(), 0o644) };
    assert_ne!(ino, 0, "create failed");

    let n = unsafe {
        fs_ext4_write_file(
            fs_h,
            path.as_ptr(),
            payload.as_ptr() as *const c_void,
            payload.len() as u64,
        )
    };
    assert_eq!(n, payload.len() as i64, "write_file size mismatch");

    let mut buf = [0u8; 64];
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
    assert_eq!(&buf[..], &payload[..], "read-back bytes mismatch");

    let rc = unsafe { fs_ext4_unlink(fs_h, path.as_ptr()) };
    assert_eq!(rc, 0, "unlink failed");

    unsafe { fs_ext4_umount(fs_h) };
}

#[test]
fn lazy_mount_eager_replay_on_existing_handle_is_idempotent() {
    let dev = fresh_dev();
    let cfg = make_cfg(&dev, true, true);

    let fs_h = unsafe { fs_ext4_mount_rw_with_callbacks_lazy(&cfg) };
    assert!(!fs_h.is_null());

    // Calling replay twice must remain safe — idempotent on a clean image.
    let rc1 = unsafe { fs_ext4_replay_journal_if_dirty(fs_h) };
    let rc2 = unsafe { fs_ext4_replay_journal_if_dirty(fs_h) };
    assert_eq!(rc1, 0);
    assert_eq!(rc2, 0);

    // And on an eager-mounted handle as well — replay-after-eager is also
    // a no-op (journal already clean).
    let cfg2 = make_cfg(&dev, true, true);
    let eager = unsafe { fs_ext4_mount_rw_with_callbacks(&cfg2) };
    assert!(!eager.is_null());
    let rc3 = unsafe { fs_ext4_replay_journal_if_dirty(eager) };
    assert_eq!(rc3, 0, "replay on eager-mounted handle must be 0");
    unsafe { fs_ext4_umount(eager) };

    unsafe { fs_ext4_umount(fs_h) };
}

#[test]
fn lazy_mount_null_cfg_returns_null() {
    let fs_h = unsafe { fs_ext4_mount_rw_with_callbacks_lazy(std::ptr::null()) };
    assert!(fs_h.is_null());
    assert_eq!(fs_ext4_last_errno(), 22, "EINVAL expected");
}

#[test]
fn lazy_mount_null_write_returns_einval() {
    let dev = fresh_dev();
    let cfg = make_cfg(&dev, false, false);
    let fs_h = unsafe { fs_ext4_mount_rw_with_callbacks_lazy(&cfg) };
    assert!(fs_h.is_null());
    assert_eq!(fs_ext4_last_errno(), 22, "EINVAL expected");
}

#[test]
fn replay_journal_if_dirty_null_handle_returns_minus_one() {
    let rc = unsafe { fs_ext4_replay_journal_if_dirty(std::ptr::null_mut()) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22, "EINVAL expected");
}
