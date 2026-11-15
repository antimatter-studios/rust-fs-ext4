//! Integration tests for `fs_ext4_removexattr` (in-inode path).
//!
//! Uses `test-disks/ext4-xattr.img` which has:
//!   /tagged.txt   — user.color=red, user.com.apple.FinderInfo=0xDEADBEEF
//!   /tagged_dir/  — user.purpose=documents
//!   /plain.txt    — no xattrs

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-xattr.img");

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_rmxattr_{tag}_{}_{n}.img",
        std::process::id()
    ));
    let bytes = fs_ext4_test_support::read_small_file(SRC).expect("read src");
    let mut out = fs::File::create(&dst).expect("create");
    out.write_all(&bytes).expect("write");
    out.flush().expect("flush");
    dst
}

fn last_err() -> String {
    unsafe {
        CStr::from_ptr(fs_ext4_last_error())
            .to_string_lossy()
            .into_owned()
    }
}

fn list_xattrs(fs: *mut fs_ext4_fs_t, path: &str) -> Vec<String> {
    let p = CString::new(path).unwrap();
    let probe = unsafe { fs_ext4_listxattr(fs, p.as_ptr(), std::ptr::null_mut(), 0) };
    assert!(probe >= 0, "listxattr probe: {}", last_err());
    if probe == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; probe as usize];
    let got = unsafe { fs_ext4_listxattr(fs, p.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len()) };
    assert!(got >= 0);
    buf.split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| std::str::from_utf8(s).unwrap().to_string())
        .collect()
}

#[test]
fn remove_existing_xattr_succeeds() {
    let img = scratch("exist");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/tagged.txt").unwrap();
    let name_c = CString::new("user.color").unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null(), "mount_rw: {}", last_err());

    let before = list_xattrs(fs_h, "/tagged.txt");
    assert!(
        before.iter().any(|n| n == "user.color"),
        "expected user.color in {before:?}"
    );

    let rc = unsafe { fs_ext4_removexattr(fs_h, path_c.as_ptr(), name_c.as_ptr()) };
    assert_eq!(rc, 0, "removexattr: {}", last_err());
    assert_eq!(fs_ext4_last_errno(), 0);

    let after = list_xattrs(fs_h, "/tagged.txt");
    assert!(
        !after.iter().any(|n| n == "user.color"),
        "user.color should be gone, got {after:?}"
    );
    // FinderInfo should still be there.
    assert!(
        after.iter().any(|n| n == "user.com.apple.FinderInfo"),
        "FinderInfo should survive, got {after:?}"
    );

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn remove_xattr_persists_across_remount() {
    let img = scratch("remount");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/tagged.txt").unwrap();
    let name_c = CString::new("user.color").unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let rc = unsafe { fs_ext4_removexattr(fs_h, path_c.as_ptr(), name_c.as_ptr()) };
    assert_eq!(rc, 0);
    unsafe { fs_ext4_umount(fs_h) };

    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount failed — csum not patched?");
    let after = list_xattrs(fs2, "/tagged.txt");
    assert!(!after.iter().any(|n| n == "user.color"));
    unsafe { fs_ext4_umount(fs2) };
    let _ = fs::remove_file(&img);
}

#[test]
fn remove_missing_xattr_returns_enoent() {
    let img = scratch("missing");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/tagged.txt").unwrap();
    let name_c = CString::new("user.does_not_exist").unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let rc = unsafe { fs_ext4_removexattr(fs_h, path_c.as_ptr(), name_c.as_ptr()) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 2, "ENOENT expected");
    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn remove_on_file_without_xattrs_returns_enoent() {
    let img = scratch("plain");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/plain.txt").unwrap();
    let name_c = CString::new("user.anything").unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let rc = unsafe { fs_ext4_removexattr(fs_h, path_c.as_ptr(), name_c.as_ptr()) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 2);
    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn remove_unknown_namespace_prefix_returns_einval() {
    let img = scratch("badns");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/tagged.txt").unwrap();
    let name_c = CString::new("weird.prefix").unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let rc = unsafe { fs_ext4_removexattr(fs_h, path_c.as_ptr(), name_c.as_ptr()) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22);
    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn remove_null_args_return_einval() {
    let img = scratch("null");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let name_c = CString::new("user.x").unwrap();
    let path_c = CString::new("/tagged.txt").unwrap();
    let rc = unsafe { fs_ext4_removexattr(fs_h, std::ptr::null(), name_c.as_ptr()) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22);
    let rc = unsafe { fs_ext4_removexattr(fs_h, path_c.as_ptr(), std::ptr::null()) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22);
    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}
