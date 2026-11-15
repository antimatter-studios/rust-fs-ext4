//! All write ops must refuse cleanly on a read-only (callback) mount.
//! Completes the RO-safety coverage already established for truncate +
//! unlink in capi_truncate_callback_mount.rs, adding create, mkdir,
//! rmdir, rename, write_file.

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::os::raw::c_void;

const IMAGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

extern "C" fn read_from_vec(
    ctx: *mut c_void,
    buf: *mut c_void,
    offset: u64,
    length: u64,
) -> std::os::raw::c_int {
    if ctx.is_null() || buf.is_null() {
        return 1;
    }
    let bytes = unsafe { &*(ctx as *const Vec<u8>) };
    let end = (offset as usize).checked_add(length as usize);
    if end.is_none_or(|e| e > bytes.len()) {
        return 2;
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

fn mount_ro(bytes: &Vec<u8>) -> *mut fs_ext4_fs_t {
    let cfg = fs_ext4_blockdev_cfg_t {
        read: Some(read_from_vec),
        context: bytes as *const Vec<u8> as *mut c_void,
        size_bytes: bytes.len() as u64,
        block_size: 512,
        write: None,
        flush: None,
    };
    let fs_h = unsafe { fs_ext4_mount_with_callbacks(&cfg) };
    assert!(!fs_h.is_null());
    fs_h
}

fn last_err() -> String {
    unsafe {
        CStr::from_ptr(fs_ext4_last_error())
            .to_string_lossy()
            .into_owned()
    }
}

#[test]
fn create_on_ro_mount_refused_with_erofs() {
    let bytes = fs_ext4_test_support::read_small_file(IMAGE).unwrap();
    let fs_h = mount_ro(&bytes);
    let p = CString::new("/new.txt").unwrap();
    let ino = unsafe { fs_ext4_create(fs_h, p.as_ptr(), 0o644) };
    assert_eq!(ino, 0, "create on RO mount must fail");
    assert_eq!(fs_ext4_last_errno(), 30, "EROFS (30) expected on RO mount");
    assert!(!last_err().is_empty());
    unsafe { fs_ext4_umount(fs_h) };
}

#[test]
fn mkdir_on_ro_mount_refused() {
    let bytes = fs_ext4_test_support::read_small_file(IMAGE).unwrap();
    let fs_h = mount_ro(&bytes);
    let p = CString::new("/newdir").unwrap();
    let ino = unsafe { fs_ext4_mkdir(fs_h, p.as_ptr(), 0o755) };
    assert_eq!(ino, 0);
    assert_ne!(fs_ext4_last_errno(), 0);
    unsafe { fs_ext4_umount(fs_h) };
}

#[test]
fn rmdir_on_ro_mount_refused() {
    let bytes = fs_ext4_test_support::read_small_file(IMAGE).unwrap();
    let fs_h = mount_ro(&bytes);
    let p = CString::new("/subdir").unwrap();
    let rc = unsafe { fs_ext4_rmdir(fs_h, p.as_ptr()) };
    assert_eq!(rc, -1);
    assert_ne!(fs_ext4_last_errno(), 0);
    unsafe { fs_ext4_umount(fs_h) };
}

#[test]
fn rename_on_ro_mount_refused() {
    let bytes = fs_ext4_test_support::read_small_file(IMAGE).unwrap();
    let fs_h = mount_ro(&bytes);
    let src = CString::new("/test.txt").unwrap();
    let dst = CString::new("/renamed.txt").unwrap();
    let rc = unsafe { fs_ext4_rename(fs_h, src.as_ptr(), dst.as_ptr()) };
    assert_eq!(rc, -1);
    assert_ne!(fs_ext4_last_errno(), 0);
    unsafe { fs_ext4_umount(fs_h) };
}

#[test]
fn write_file_on_ro_mount_refused_and_data_intact() {
    let bytes = fs_ext4_test_support::read_small_file(IMAGE).unwrap();
    let fs_h = mount_ro(&bytes);
    let p = CString::new("/test.txt").unwrap();
    let payload = b"nope";
    let rc = unsafe {
        fs_ext4_write_file(
            fs_h,
            p.as_ptr(),
            payload.as_ptr() as *const c_void,
            payload.len() as u64,
        )
    };
    assert_eq!(rc, -1);
    assert_ne!(fs_ext4_last_errno(), 0);

    // Confirm content wasn't corrupted.
    let mut buf = [0u8; 32];
    let n = unsafe {
        fs_ext4_read_file(
            fs_h,
            p.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            0,
            buf.len() as u64,
        )
    };
    assert_eq!(n, 16);
    assert_eq!(&buf[..16], b"hello from ext4\n");

    unsafe { fs_ext4_umount(fs_h) };
}
