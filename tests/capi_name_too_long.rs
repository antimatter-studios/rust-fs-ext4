//! Verify names longer than EXT4_NAME_LEN (255 bytes) are refused with
//! POSIX ENAMETOOLONG (63) across the write-path C ABI.

use fs_ext4::capi::*;
use std::ffi::CString;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

fn scratch() -> PathBuf {
    static C: AtomicU32 = AtomicU32::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_nametoolong_{}_{n}.img",
        std::process::id()
    ));
    fs::copy(SRC, &dst).unwrap();
    dst
}

fn long_name(n: usize) -> String {
    // 256-char name (1 byte over EXT4_NAME_LEN=255).
    let mut s = String::from("/");
    for _ in 0..n {
        s.push('a');
    }
    s
}

#[test]
fn create_with_name_longer_than_255_returns_enametoolong() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let path = CString::new(long_name(256)).unwrap();
    let ino = unsafe { fs_ext4_create(fs_h, path.as_ptr(), 0o644) };
    assert_eq!(ino, 0, "create with 256-char name must fail");
    assert_eq!(fs_ext4_last_errno(), 63, "ENAMETOOLONG expected");

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn mkdir_with_name_longer_than_255_returns_enametoolong() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let path = CString::new(long_name(300)).unwrap();
    let ino = unsafe { fs_ext4_mkdir(fs_h, path.as_ptr(), 0o755) };
    assert_eq!(ino, 0);
    assert_eq!(fs_ext4_last_errno(), 63, "ENAMETOOLONG expected");

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn name_at_the_limit_is_accepted() {
    // Exactly 255 chars — should work.
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let path = CString::new(long_name(255)).unwrap();
    let ino = unsafe { fs_ext4_create(fs_h, path.as_ptr(), 0o644) };
    assert_ne!(ino, 0, "255-char name should be accepted");
    assert_eq!(fs_ext4_last_errno(), 0);

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}
