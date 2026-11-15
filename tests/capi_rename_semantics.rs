//! Rename semantics through the C ABI.
//!
//! Current fs_ext4_rename is no-replace — if the destination exists
//! the op fails rather than overwriting atomically (POSIX rename(2)
//! normally overwrites). That's a safer default for now; POSIX
//! atomic-replace is deferred. Tests lock in the current contract.

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::Write;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

fn scratch(label: &str) -> PathBuf {
    static C: AtomicU32 = AtomicU32::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_rename_{label}_{}_{n}.img",
        std::process::id()
    ));
    let mut out = fs::File::create(&dst).unwrap();
    out.write_all(&fs_ext4_test_support::read_small_file(SRC).unwrap())
        .unwrap();
    dst
}

fn last_err() -> String {
    unsafe {
        CStr::from_ptr(fs_ext4_last_error())
            .to_string_lossy()
            .into_owned()
    }
}

#[test]
fn rename_self_is_a_noop_success() {
    let img = scratch("self");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let p = CString::new("/test.txt").unwrap();
    let rc = unsafe { fs_ext4_rename(fs_h, p.as_ptr(), p.as_ptr()) };
    assert_eq!(rc, 0, "rename(x,x) must be no-op success: {}", last_err());
    assert_eq!(fs_ext4_last_errno(), 0);

    // File still there and readable.
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
    let _ = fs::remove_file(&img);
}

#[test]
fn rename_to_existing_file_fails_with_eexist() {
    let img = scratch("to_existing");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let newfile = CString::new("/destination.txt").unwrap();
    let ino = unsafe { fs_ext4_create(fs_h, newfile.as_ptr(), 0o644) };
    assert_ne!(ino, 0);

    let src = CString::new("/test.txt").unwrap();
    let rc = unsafe { fs_ext4_rename(fs_h, src.as_ptr(), newfile.as_ptr()) };
    assert_eq!(rc, -1, "rename to existing must fail");
    assert_eq!(fs_ext4_last_errno(), 17, "EEXIST expected");

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn rename_across_directories_works_and_persists() {
    let img = scratch("across");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let src = CString::new("/test.txt").unwrap();
    let dst = CString::new("/subdir/moved.txt").unwrap();

    {
        let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
        assert!(!fs_h.is_null());
        let rc = unsafe { fs_ext4_rename(fs_h, src.as_ptr(), dst.as_ptr()) };
        assert_eq!(rc, 0, "cross-dir rename: {}", last_err());
        unsafe { fs_ext4_umount(fs_h) };
    }

    // Remount ro and verify.
    {
        let fs_h = unsafe { fs_ext4_mount(img_c.as_ptr()) };
        assert!(!fs_h.is_null());

        // Source gone.
        let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
        let rc = unsafe { fs_ext4_stat(fs_h, src.as_ptr(), &mut attr) };
        assert_eq!(rc, -1);
        assert_eq!(fs_ext4_last_errno(), 2);

        // Dest present and same content.
        let rc = unsafe { fs_ext4_stat(fs_h, dst.as_ptr(), &mut attr) };
        assert_eq!(rc, 0, "stat dst: {}", last_err());
        assert_eq!(attr.size, 16);

        let mut buf = [0u8; 32];
        let n = unsafe {
            fs_ext4_read_file(
                fs_h,
                dst.as_ptr(),
                buf.as_mut_ptr() as *mut c_void,
                0,
                buf.len() as u64,
            )
        };
        assert_eq!(n, 16);
        assert_eq!(&buf[..16], b"hello from ext4\n");

        unsafe { fs_ext4_umount(fs_h) };
    }

    let _ = fs::remove_file(&img);
}

#[test]
fn rename_dst_parent_missing_returns_enoent() {
    let img = scratch("parent_missing");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let src = CString::new("/test.txt").unwrap();
    let dst = CString::new("/nonexistent_dir/new.txt").unwrap();
    let rc = unsafe { fs_ext4_rename(fs_h, src.as_ptr(), dst.as_ptr()) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 2, "ENOENT for missing parent");

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn rename_directory_into_own_subtree_returns_einval() {
    // Classic POSIX trap: rename("/a", "/a/b") — would create an unreachable
    // loop. Kernel refuses with EINVAL.
    let img = scratch("own_subtree");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    // /subdir exists as a directory. Try renaming it under itself.
    let src = CString::new("/subdir").unwrap();
    let dst = CString::new("/subdir/moved").unwrap();
    let rc = unsafe { fs_ext4_rename(fs_h, src.as_ptr(), dst.as_ptr()) };
    assert_eq!(rc, -1, "rename into own subtree must fail");
    assert_eq!(fs_ext4_last_errno(), 22, "EINVAL expected");

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn rename_null_args_return_einval() {
    let rc = unsafe { fs_ext4_rename(std::ptr::null_mut(), std::ptr::null(), std::ptr::null()) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22);
}
