//! C-ABI tests for the write-path surface: `fs_ext4_mount_rw` and
//! `fs_ext4_truncate`. Exercises the real path Swift FSKit will call.
//!
//! We copy `ext4-basic.img` into the selected scratch directory for each test so the shared test
//! disk in the repo stays read-only and tests don't interfere.

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC_IMAGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

fn last_err_str() -> String {
    unsafe {
        let p = fs_ext4_last_error();
        if p.is_null() {
            return "<null>".into();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// Fresh writable copy of the basic image. Caller owns the path and is
/// responsible for `std::fs::remove_file` on it.
fn scratch_image() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_truncate_{}_{n}.img",
        std::process::id()
    ));
    let bytes = fs_ext4_test_support::read_small_file(SRC_IMAGE).expect("read src image");
    let mut out = std::fs::File::create(&dst).expect("create dst image");
    out.write_all(&bytes).expect("write dst image");
    out.flush().expect("flush");
    drop(out);
    dst
}

fn stat_size(fs: *mut fs_ext4_fs_t, path: &str) -> u64 {
    let p = CString::new(path).unwrap();
    // Zero-initialize via MaybeUninit since the C struct has no Default impl
    // (it is `#[repr(C)]` and mirrors the Swift side's zero-init convention).
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_ext4_stat(fs, p.as_ptr(), &mut attr as *mut _) };
    assert_eq!(rc, 0, "stat {path}: {}", last_err_str());
    attr.size
}

#[test]
fn mount_rw_then_truncate_shrinks_and_persists() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    // First: R/W mount + stat the original size.
    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let original = stat_size(fs, "/test.txt");
    assert!(original > 0, "original size should be non-zero");

    // Shrink to half. `apply_truncate_shrink` rounds to zero for sizes below
    // the first extent boundary, but any value <= original is legal as input.
    let target = original / 2;
    let rc = unsafe { fs_ext4_truncate(fs, path_c.as_ptr(), target) };
    assert_eq!(rc, 0, "truncate: {}", last_err_str());
    assert_eq!(
        stat_size(fs, "/test.txt"),
        target,
        "size after truncate (pre-remount)"
    );

    unsafe { fs_ext4_umount(fs) };

    // Re-mount RO and confirm the new size persisted.
    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount: {}", last_err_str());
    assert_eq!(stat_size(fs2, "/test.txt"), target, "size after remount");
    unsafe { fs_ext4_umount(fs2) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn truncate_to_zero_clears_file() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let rc = unsafe { fs_ext4_truncate(fs, path_c.as_ptr(), 0) };
    assert_eq!(rc, 0, "truncate to 0: {}", last_err_str());
    assert_eq!(stat_size(fs, "/test.txt"), 0);
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn truncate_on_ro_mount_returns_minus_one() {
    let img_c = CString::new(SRC_IMAGE).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    // RO mount — truncate must refuse.
    let fs = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount: {}", last_err_str());
    let rc = unsafe { fs_ext4_truncate(fs, path_c.as_ptr(), 0) };
    assert_eq!(rc, -1, "truncate on RO mount must fail");
    let err = last_err_str();
    assert!(
        err.contains("read-only") || err.contains("apply_truncate_shrink"),
        "error should mention read-only: got {err}"
    );
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn truncate_growing_succeeds_via_sparse_path() {
    // Regression: earlier this test asserted that grow was REJECTED. The
    // sparse-grow path is now implemented (see capi_truncate_grow.rs for the
    // full suite). This test holds the line that the old rejection is gone
    // and the size grows when asked.
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let original = stat_size(fs, "/test.txt");

    let rc = unsafe { fs_ext4_truncate(fs, path_c.as_ptr(), original + 4096) };
    assert_eq!(rc, 0, "grow-truncate now succeeds via sparse path");
    assert_eq!(
        stat_size(fs, "/test.txt"),
        original + 4096,
        "size must reflect grow"
    );

    unsafe { fs_ext4_umount(fs) };
    std::fs::remove_file(&img).ok();
}

#[test]
fn truncate_on_null_inputs_does_not_crash() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    // null fs
    let path_c = CString::new("/test.txt").unwrap();
    let rc = unsafe { fs_ext4_truncate(std::ptr::null_mut(), path_c.as_ptr(), 0) };
    assert_eq!(rc, -1);

    // null path
    let rc = unsafe { fs_ext4_truncate(fs, std::ptr::null(), 0) };
    assert_eq!(rc, -1);

    // missing path
    let bad = CString::new("/does-not-exist.txt").unwrap();
    let rc = unsafe { fs_ext4_truncate(fs, bad.as_ptr(), 0) };
    assert_eq!(rc, -1);

    unsafe { fs_ext4_umount(fs) };
    std::fs::remove_file(&img).ok();
}
