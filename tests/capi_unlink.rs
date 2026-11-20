//! C-ABI tests for `fs_ext4_unlink`. Each test makes its own scratch
//! copy of `ext4-basic.img` so the shared test disk stays clean.

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
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

fn scratch_image() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_unlink_{}_{n}.img",
        std::process::id()
    ));
    std::fs::copy(SRC_IMAGE, &dst).expect("copy src image");
    dst
}

fn path_exists(fs: *mut fs_ext4_fs_t, path: &str) -> bool {
    let p = CString::new(path).unwrap();
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    unsafe { fs_ext4_stat(fs, p.as_ptr(), &mut attr as *mut _) == 0 }
}

#[test]
fn unlink_regular_file_removes_entry_and_persists() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    assert!(
        path_exists(fs, "/test.txt"),
        "/test.txt should exist before unlink"
    );

    let rc = unsafe { fs_ext4_unlink(fs, path_c.as_ptr()) };
    assert_eq!(rc, 0, "unlink: {}", last_err_str());
    assert!(
        !path_exists(fs, "/test.txt"),
        "/test.txt should not stat after unlink"
    );

    unsafe { fs_ext4_umount(fs) };

    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount: {}", last_err_str());
    assert!(
        !path_exists(fs2, "/test.txt"),
        "/test.txt should not come back after remount"
    );
    // Sibling entries must still be there (we didn't trash the dir).
    assert!(path_exists(fs2, "/subdir"), "/subdir should survive unlink");
    unsafe { fs_ext4_umount(fs2) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn unlink_refuses_directory() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/subdir").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let rc = unsafe { fs_ext4_unlink(fs, path_c.as_ptr()) };
    assert_eq!(rc, -1, "unlink of dir must fail");
    let err = last_err_str();
    assert!(
        err.contains("directory") || err.contains("EISDIR"),
        "error should mention directory: {err}"
    );
    // Dir must still be present.
    assert!(path_exists(fs, "/subdir"));
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn unlink_on_ro_mount_returns_minus_one() {
    let img_c = CString::new(SRC_IMAGE).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount: {}", last_err_str());
    let rc = unsafe { fs_ext4_unlink(fs, path_c.as_ptr()) };
    assert_eq!(rc, -1, "unlink on RO mount must fail");
    assert!(path_exists(fs, "/test.txt"), "file should be untouched");
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn unlink_missing_path_returns_minus_one() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/this-is-not-a-real-file").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let rc = unsafe { fs_ext4_unlink(fs, path_c.as_ptr()) };
    assert_eq!(rc, -1, "unlink missing path must fail");
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn unlink_symlink_frees_inode() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/link.txt").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    assert!(path_exists(fs, "/link.txt"), "/link.txt should exist");

    let rc = unsafe { fs_ext4_unlink(fs, path_c.as_ptr()) };
    assert_eq!(rc, 0, "unlink symlink: {}", last_err_str());
    assert!(!path_exists(fs, "/link.txt"), "/link.txt gone");
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn unlink_null_inputs_do_not_crash() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let path_c = CString::new("/test.txt").unwrap();
    let rc = unsafe { fs_ext4_unlink(std::ptr::null_mut(), path_c.as_ptr()) };
    assert_eq!(rc, -1);
    let rc = unsafe { fs_ext4_unlink(fs, std::ptr::null()) };
    assert_eq!(rc, -1);

    unsafe { fs_ext4_umount(fs) };
    std::fs::remove_file(&img).ok();
}
