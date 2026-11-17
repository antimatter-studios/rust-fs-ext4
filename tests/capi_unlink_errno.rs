//! Errno-semantic complement to @6's capi_unlink.rs.
//!
//! @6's tests verify unlink's success path + message content on failure.
//! This adds assertions on `fs_ext4_last_errno` so the Swift side can
//! rely on POSIXErrorDomain codes for NSError construction.

use fs_ext4::capi::*;
use std::ffi::CString;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

fn scratch() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_unlink_errno_{}_{n}.img",
        std::process::id()
    ));
    fs::copy(SRC, &dst).expect("copy src");
    dst
}

#[test]
fn unlink_on_success_clears_errno() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let rc = unsafe { fs_ext4_unlink(fs_handle, path_c.as_ptr()) };
    assert_eq!(rc, 0);
    assert_eq!(fs_ext4_last_errno(), 0, "success must leave errno=0");
    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn unlink_missing_path_sets_enoent() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let bad = CString::new("/does-not-exist-xyz").unwrap();
    let rc = unsafe { fs_ext4_unlink(fs_handle, bad.as_ptr()) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 2, "ENOENT expected for missing path");
    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn unlink_directory_sets_eisdir() {
    // POSIX: unlink(2) on a directory must fail with EISDIR.
    // Fixed by @6 via new Error::IsADirectory variant mapping to EISDIR.
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let dir = CString::new("/subdir").unwrap();
    let rc = unsafe { fs_ext4_unlink(fs_handle, dir.as_ptr()) };
    assert_eq!(rc, -1);
    assert_eq!(
        fs_ext4_last_errno(),
        21,
        "expected EISDIR for unlink-on-dir"
    );
    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn unlink_null_fs_sets_einval() {
    let path = CString::new("/test.txt").unwrap();
    let rc = unsafe { fs_ext4_unlink(std::ptr::null_mut(), path.as_ptr()) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22); // EINVAL
}

#[test]
fn double_unlink_second_call_is_enoent() {
    // After a successful unlink, the file is gone; calling unlink again
    // on the same path should fail with ENOENT.
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let path = CString::new("/test.txt").unwrap();

    let rc1 = unsafe { fs_ext4_unlink(fs_handle, path.as_ptr()) };
    assert_eq!(rc1, 0, "first unlink");
    let rc2 = unsafe { fs_ext4_unlink(fs_handle, path.as_ptr()) };
    assert_eq!(rc2, -1, "second unlink MUST fail");
    assert_eq!(fs_ext4_last_errno(), 2, "second unlink errno=ENOENT");

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn unlink_root_fails() {
    // Never let a caller unlink the root directory — that would orphan the fs.
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let root = CString::new("/").unwrap();
    let rc = unsafe { fs_ext4_unlink(fs_handle, root.as_ptr()) };
    assert_eq!(rc, -1, "unlink root MUST fail");
    let e = fs_ext4_last_errno();
    assert_ne!(e, 0, "root unlink must set non-zero errno");
    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn unlink_null_path_sets_einval() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let rc = unsafe { fs_ext4_unlink(fs_handle, std::ptr::null()) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22); // EINVAL
    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}
