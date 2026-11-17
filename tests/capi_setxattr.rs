//! Integration tests for `fs_ext4_setxattr` (in-inode path).
//!
//! Uses `test-disks/ext4-xattr.img` which has an existing tagged file at
//! `/tagged.txt` (user.color=red, user.com.apple.FinderInfo) and a plain
//! file at `/plain.txt` with no xattrs.

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

#[track_caller]
fn scratch(tag: &str) -> PathBuf {
    let src = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), "ext4-xattr.img");
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_setxattr_{tag}_{}_{n}.img",
        std::process::id()
    ));
    fs::copy(&src, &dst).expect("copy src");
    dst
}

fn last_err() -> String {
    unsafe {
        CStr::from_ptr(fs_ext4_last_error())
            .to_string_lossy()
            .into_owned()
    }
}

fn get_xattr(fs: *mut fs_ext4_fs_t, path: &str, name: &str) -> Option<Vec<u8>> {
    let p = CString::new(path).unwrap();
    let n = CString::new(name).unwrap();
    let probe = unsafe { fs_ext4_getxattr(fs, p.as_ptr(), n.as_ptr(), std::ptr::null_mut(), 0) };
    if probe < 0 {
        return None;
    }
    if probe == 0 {
        return Some(Vec::new());
    }
    let mut buf = vec![0u8; probe as usize];
    let got = unsafe {
        fs_ext4_getxattr(
            fs,
            p.as_ptr(),
            n.as_ptr(),
            buf.as_mut_ptr() as *mut _,
            buf.len(),
        )
    };
    if got < 0 {
        return None;
    }
    Some(buf[..got as usize].to_vec())
}

#[test]
fn setxattr_creates_new_entry_on_plain_file() {
    let img = scratch("create");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/plain.txt").unwrap();
    let name_c = CString::new("user.tag").unwrap();
    let value = b"review";

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null(), "mount_rw: {}", last_err());

    let rc = unsafe {
        fs_ext4_setxattr(
            fs_h,
            path_c.as_ptr(),
            name_c.as_ptr(),
            value.as_ptr() as *const _,
            value.len(),
        )
    };
    assert_eq!(rc, 0, "setxattr: {}", last_err());
    assert_eq!(fs_ext4_last_errno(), 0);

    let got = get_xattr(fs_h, "/plain.txt", "user.tag").unwrap();
    assert_eq!(got, value);

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn setxattr_replaces_existing_entry() {
    let img = scratch("replace");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/tagged.txt").unwrap();
    let name_c = CString::new("user.color").unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    // Before: user.color=red.
    assert_eq!(
        get_xattr(fs_h, "/tagged.txt", "user.color").unwrap(),
        b"red"
    );

    // Same-length replacement — original value was "red" (3B → padded 4).
    // "sky" is also 3B, so the layout fits regardless of how tight the
    // original region was packed.
    let new = b"sky";
    let rc = unsafe {
        fs_ext4_setxattr(
            fs_h,
            path_c.as_ptr(),
            name_c.as_ptr(),
            new.as_ptr() as *const _,
            new.len(),
        )
    };
    assert_eq!(rc, 0, "setxattr: {}", last_err());

    assert_eq!(
        get_xattr(fs_h, "/tagged.txt", "user.color").unwrap(),
        b"sky"
    );
    // Other xattr (FinderInfo) must survive.
    assert!(get_xattr(fs_h, "/tagged.txt", "user.com.apple.FinderInfo").is_some());

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn setxattr_persists_across_remount() {
    let img = scratch("remount");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/plain.txt").unwrap();
    let name_c = CString::new("user.label").unwrap();
    let value = b"persisted";

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let rc = unsafe {
        fs_ext4_setxattr(
            fs_h,
            path_c.as_ptr(),
            name_c.as_ptr(),
            value.as_ptr() as *const _,
            value.len(),
        )
    };
    assert_eq!(rc, 0);
    unsafe { fs_ext4_umount(fs_h) };

    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount failed — csum not patched?");
    let got = get_xattr(fs2, "/plain.txt", "user.label").unwrap();
    assert_eq!(got, value);
    unsafe { fs_ext4_umount(fs2) };
    let _ = fs::remove_file(&img);
}

#[test]
fn setxattr_unknown_prefix_returns_einval() {
    let img = scratch("badns");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/plain.txt").unwrap();
    let name_c = CString::new("strange.name").unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let value = b"x";
    let rc = unsafe {
        fs_ext4_setxattr(
            fs_h,
            path_c.as_ptr(),
            name_c.as_ptr(),
            value.as_ptr() as *const _,
            value.len(),
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22);
    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn setxattr_spills_to_external_block_when_inline_full() {
    // A 512-byte value cannot fit in the in-inode region (which maxes
    // out around ~96 bytes); after Phase 3 it should spill into an
    // external xattr block (allocated on demand) rather than ENOSPC.
    let img = scratch("spill_external");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/plain.txt").unwrap();
    let name_c = CString::new("user.huge").unwrap();
    let value = vec![0xABu8; 512];

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let rc = unsafe {
        fs_ext4_setxattr(
            fs_h,
            path_c.as_ptr(),
            name_c.as_ptr(),
            value.as_ptr() as *const _,
            value.len(),
        )
    };
    assert_eq!(
        rc,
        0,
        "set spilling to external block should succeed (errno={})",
        fs_ext4_last_errno()
    );

    // Read back via getxattr to confirm round-trip through the block.
    let mut out = vec![0u8; value.len()];
    let n = unsafe {
        fs_ext4::capi::fs_ext4_getxattr(
            fs_h,
            path_c.as_ptr(),
            name_c.as_ptr(),
            out.as_mut_ptr() as *mut _,
            out.len(),
        )
    };
    assert_eq!(n, value.len() as i64, "readback length mismatch");
    assert_eq!(out, value, "external xattr block round-trip corrupted");

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn setxattr_oversized_value_returns_enospc() {
    // A value that exceeds the external-block capacity (~4060 bytes for a
    // 4 KiB block, accounting for the 32-byte header + entry overhead)
    // should still surface ENOSPC.
    let img = scratch("oversized");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/plain.txt").unwrap();
    let name_c = CString::new("user.toobig").unwrap();
    let value = vec![0xCDu8; 8192];

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let rc = unsafe {
        fs_ext4_setxattr(
            fs_h,
            path_c.as_ptr(),
            name_c.as_ptr(),
            value.as_ptr() as *const _,
            value.len(),
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 28, "ENOSPC expected for oversized");
    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn setxattr_null_args_return_einval() {
    let img = scratch("null");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let p = CString::new("/plain.txt").unwrap();
    let n = CString::new("user.x").unwrap();
    let rc = unsafe {
        fs_ext4_setxattr(
            fs_h,
            std::ptr::null(),
            n.as_ptr(),
            b"v".as_ptr() as *const _,
            1,
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22);
    let rc = unsafe {
        fs_ext4_setxattr(
            fs_h,
            p.as_ptr(),
            std::ptr::null(),
            b"v".as_ptr() as *const _,
            1,
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22);
    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}
