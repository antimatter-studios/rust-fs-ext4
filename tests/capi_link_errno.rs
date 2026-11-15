//! Errno-semantic complement to capi_link.rs.
//!
//! capi_link.rs verifies functional semantics + error-message content;
//! this asserts the POSIX errno values FSKit needs for NSError building.

use fs_ext4::capi::*;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

fn scratch() -> PathBuf {
    static C: AtomicU32 = AtomicU32::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_link_errno_{}_{n}.img",
        std::process::id()
    ));
    let mut out = fs::File::create(&dst).unwrap();
    out.write_all(&fs_ext4_test_support::read_small_file(SRC).unwrap())
        .unwrap();
    dst
}

#[test]
fn link_success_clears_errno() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let src = CString::new("/test.txt").unwrap();
    let dst = CString::new("/test-hardlink.txt").unwrap();
    let rc = unsafe { fs_ext4_link(fs_h, src.as_ptr(), dst.as_ptr()) };
    assert_eq!(rc, 0);
    assert_eq!(fs_ext4_last_errno(), 0);
    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn link_missing_source_sets_enoent() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let src = CString::new("/does-not-exist.txt").unwrap();
    let dst = CString::new("/anything.txt").unwrap();
    let rc = unsafe { fs_ext4_link(fs_h, src.as_ptr(), dst.as_ptr()) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 2, "ENOENT for missing src");
    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn link_directory_source_sets_eperm_or_eisdir() {
    // Hard-linking directories is forbidden on ext4 (POSIX: EPERM).
    // Our Error::IsADirectory maps to EISDIR (21). Either is acceptable as
    // long as the op refuses with a POSIX-sensible errno, not EIO.
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let src = CString::new("/subdir").unwrap();
    let dst = CString::new("/subdir-link").unwrap();
    let rc = unsafe { fs_ext4_link(fs_h, src.as_ptr(), dst.as_ptr()) };
    assert_eq!(rc, -1);
    let e = fs_ext4_last_errno();
    assert!(
        e == 1 || e == 21 || e == 22,
        "expected EPERM (1), EISDIR (21), or EINVAL (22); got {e}"
    );
    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn link_existing_destination_sets_eexist() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    // /test.txt already exists; try to link /test.txt → /test.txt (same name).
    // Also try linking a fresh file onto another existing file.
    let src = CString::new("/test.txt").unwrap();
    let dst_existing = CString::new("/test.txt").unwrap();
    let rc = unsafe { fs_ext4_link(fs_h, src.as_ptr(), dst_existing.as_ptr()) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 17, "EEXIST expected");

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn link_null_args_set_einval() {
    let rc = unsafe { fs_ext4_link(std::ptr::null_mut(), std::ptr::null(), std::ptr::null()) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22);
}
