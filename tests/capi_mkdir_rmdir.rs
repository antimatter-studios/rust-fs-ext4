//! C-ABI tests for `fs_ext4_mkdir` and `fs_ext4_rmdir`. Covers the
//! full Finder "new folder" / "move to trash (for an empty folder)" round
//! trip, plus edge cases (existing target, non-empty rmdir, regular-file
//! rmdir refusal).

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
        "fs_ext4_capi_mkdir_rmdir_{}_{n}.img",
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

fn stat(fs: *mut fs_ext4_fs_t, path: &str) -> fs_ext4_attr_t {
    let p = CString::new(path).unwrap();
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_ext4_stat(fs, p.as_ptr(), &mut attr as *mut _) };
    assert_eq!(rc, 0, "stat {path}: {}", last_err_str());
    attr
}

#[test]
fn mkdir_creates_visible_directory_and_persists() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path = "/fresh";
    let path_c = CString::new(path).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let ino = unsafe { fs_ext4_mkdir(fs, path_c.as_ptr(), 0o755) };
    assert!(ino > 0, "mkdir: {}", last_err_str());
    assert!(path_exists(fs, path));
    let a = stat(fs, path);
    assert_eq!(a.inode, ino);
    // file_type reported as Dir
    assert_eq!(a.file_type as u8, fs_ext4_file_type_t::Dir as u8);
    // perm bits preserved
    assert_eq!(a.mode, 0o755);

    unsafe { fs_ext4_umount(fs) };

    // Persists across remount.
    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount: {}", last_err_str());
    assert!(path_exists(fs2, path));
    let a2 = stat(fs2, path);
    assert_eq!(a2.inode, ino);
    assert_eq!(a2.file_type as u8, fs_ext4_file_type_t::Dir as u8);
    unsafe { fs_ext4_umount(fs2) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn mkdir_then_create_file_inside_round_trip() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let dir_c = CString::new("/newdir").unwrap();
    let file_c = CString::new("/newdir/inside.txt").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let dir_ino = unsafe { fs_ext4_mkdir(fs, dir_c.as_ptr(), 0o755) };
    assert!(dir_ino > 0, "mkdir: {}", last_err_str());
    let file_ino = unsafe { fs_ext4_create(fs, file_c.as_ptr(), 0o644) };
    assert!(file_ino > 0, "create in new dir: {}", last_err_str());
    assert!(path_exists(fs, "/newdir/inside.txt"));
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn mkdir_refuses_existing_path() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    // /subdir already exists on ext4-basic.img.
    let path_c = CString::new("/subdir").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let ino = unsafe { fs_ext4_mkdir(fs, path_c.as_ptr(), 0o755) };
    assert_eq!(ino, 0, "mkdir duplicate must fail");
    let err = last_err_str();
    assert!(err.contains("exist"), "expected exists in error: {err}");
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn mkdir_refuses_missing_parent() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/nope/child").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let ino = unsafe { fs_ext4_mkdir(fs, path_c.as_ptr(), 0o755) };
    assert_eq!(ino, 0);
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn mkdir_refuses_on_ro_mount() {
    let img_c = CString::new(SRC_IMAGE).unwrap();
    let path_c = CString::new("/wont_appear").unwrap();

    let fs = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount: {}", last_err_str());
    let ino = unsafe { fs_ext4_mkdir(fs, path_c.as_ptr(), 0o755) };
    assert_eq!(ino, 0);
    let err = last_err_str();
    assert!(
        err.contains("read-only") || err.contains("apply_mkdir"),
        "RO error: {err}"
    );
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn rmdir_removes_empty_directory_and_persists() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/tmpdir").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let ino = unsafe { fs_ext4_mkdir(fs, path_c.as_ptr(), 0o755) };
    assert!(ino > 0, "mkdir: {}", last_err_str());

    let rc = unsafe { fs_ext4_rmdir(fs, path_c.as_ptr()) };
    assert_eq!(rc, 0, "rmdir: {}", last_err_str());
    assert!(!path_exists(fs, "/tmpdir"));

    unsafe { fs_ext4_umount(fs) };

    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount: {}", last_err_str());
    assert!(
        !path_exists(fs2, "/tmpdir"),
        "dir should stay gone after remount"
    );
    unsafe { fs_ext4_umount(fs2) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn rmdir_refuses_non_empty_directory() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let dir_c = CString::new("/nested").unwrap();
    let file_c = CString::new("/nested/child.txt").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let d = unsafe { fs_ext4_mkdir(fs, dir_c.as_ptr(), 0o755) };
    assert!(d > 0, "mkdir: {}", last_err_str());
    let f = unsafe { fs_ext4_create(fs, file_c.as_ptr(), 0o644) };
    assert!(f > 0, "create child: {}", last_err_str());

    let rc = unsafe { fs_ext4_rmdir(fs, dir_c.as_ptr()) };
    assert_eq!(rc, -1, "rmdir on non-empty must fail");
    let err = last_err_str();
    assert!(
        err.contains("not empty") || err.contains("ENOTEMPTY"),
        "error should mention non-empty: {err}"
    );
    // Clean up the child first, THEN rmdir should succeed.
    let rc = unsafe { fs_ext4_unlink(fs, file_c.as_ptr()) };
    assert_eq!(rc, 0, "unlink child: {}", last_err_str());
    let rc = unsafe { fs_ext4_rmdir(fs, dir_c.as_ptr()) };
    assert_eq!(rc, 0, "rmdir after emptying: {}", last_err_str());
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn rmdir_refuses_regular_file() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let rc = unsafe { fs_ext4_rmdir(fs, path_c.as_ptr()) };
    assert_eq!(rc, -1);
    assert!(path_exists(fs, "/test.txt"));
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn rmdir_refuses_missing_path() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/nope").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let rc = unsafe { fs_ext4_rmdir(fs, path_c.as_ptr()) };
    assert_eq!(rc, -1);
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn rmdir_refuses_on_ro_mount() {
    let img_c = CString::new(SRC_IMAGE).unwrap();
    let path_c = CString::new("/subdir").unwrap();

    let fs = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount: {}", last_err_str());
    let rc = unsafe { fs_ext4_rmdir(fs, path_c.as_ptr()) };
    assert_eq!(rc, -1);
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn mkdir_rmdir_null_inputs_do_not_crash() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let p = CString::new("/x").unwrap();
    assert_eq!(
        unsafe { fs_ext4_mkdir(std::ptr::null_mut(), p.as_ptr(), 0o755) },
        0
    );
    assert_eq!(unsafe { fs_ext4_mkdir(fs, std::ptr::null(), 0o755) }, 0);
    assert_eq!(
        unsafe { fs_ext4_rmdir(std::ptr::null_mut(), p.as_ptr()) },
        -1
    );
    assert_eq!(unsafe { fs_ext4_rmdir(fs, std::ptr::null()) }, -1);

    unsafe { fs_ext4_umount(fs) };
    std::fs::remove_file(&img).ok();
}
