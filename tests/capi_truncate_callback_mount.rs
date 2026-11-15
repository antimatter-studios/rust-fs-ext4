//! Verify that truncate on a callback-mounted fs is refused cleanly.
//!
//! CallbackDevice has no write path, so it's read-only by construction.
//! A caller that calls fs_ext4_truncate on a callback mount should
//! get an error, not silent corruption.

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

#[test]
fn truncate_on_callback_mount_refused_cleanly() {
    let bytes = fs_ext4_test_support::read_small_file(IMAGE).expect("read image");
    let cfg = fs_ext4_blockdev_cfg_t {
        read: Some(read_from_vec),
        context: &bytes as *const Vec<u8> as *mut c_void,
        size_bytes: bytes.len() as u64,
        block_size: 512,
        write: None,
        flush: None,
    };
    let fs_h = unsafe { fs_ext4_mount_with_callbacks(&cfg) };
    assert!(!fs_h.is_null(), "callback mount");

    let path = CString::new("/test.txt").unwrap();
    let rc = unsafe { fs_ext4_truncate(fs_h, path.as_ptr(), 0) };
    assert_eq!(rc, -1, "truncate on callback (RO) mount must fail");
    assert_ne!(fs_ext4_last_errno(), 0, "errno must be set on failure");

    // Verify the data wasn't touched — read /test.txt and confirm it's intact.
    let mut buf = [0u8; 64];
    let n = unsafe {
        fs_ext4_read_file(
            fs_h,
            path.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            0,
            buf.len() as u64,
        )
    };
    assert!(n > 0);
    let content = &buf[..n as usize];
    assert_eq!(
        content, b"hello from ext4\n",
        "file must be unchanged after refused truncate"
    );

    unsafe { fs_ext4_umount(fs_h) };
}

#[test]
fn unlink_on_callback_mount_refused_cleanly() {
    let bytes = fs_ext4_test_support::read_small_file(IMAGE).expect("read image");
    let cfg = fs_ext4_blockdev_cfg_t {
        read: Some(read_from_vec),
        context: &bytes as *const Vec<u8> as *mut c_void,
        size_bytes: bytes.len() as u64,
        block_size: 512,
        write: None,
        flush: None,
    };
    let fs_h = unsafe { fs_ext4_mount_with_callbacks(&cfg) };
    assert!(!fs_h.is_null());

    let path = CString::new("/test.txt").unwrap();
    let rc = unsafe { fs_ext4_unlink(fs_h, path.as_ptr()) };
    assert_eq!(rc, -1, "unlink on callback (RO) mount must fail");
    let err = unsafe {
        CStr::from_ptr(fs_ext4_last_error())
            .to_string_lossy()
            .into_owned()
    };
    assert!(!err.is_empty());

    unsafe { fs_ext4_umount(fs_h) };
}
