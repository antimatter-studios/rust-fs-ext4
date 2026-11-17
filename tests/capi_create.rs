//! C-ABI tests for `fs_ext4_create`. Each test works on its own scratch
//! copy of `ext4-basic.img` so the shared disk stays clean.

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
        "fs_ext4_capi_create_{}_{n}.img",
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
fn create_new_file_visible_and_stats_as_zero_sized_regular() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/brand_new.txt").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let ino = unsafe { fs_ext4_create(fs, path_c.as_ptr(), 0o644) };
    assert!(ino > 0, "create returned 0: {}", last_err_str());

    assert!(path_exists(fs, "/brand_new.txt"));
    let a = stat(fs, "/brand_new.txt");
    assert_eq!(a.size, 0);
    assert_eq!(a.inode, ino);
    // `fill_attr` strips the high type bits from `mode` — only the 0o777
    // permission bits survive. Regular-file-ness shows up in `file_type`.
    assert_eq!(a.mode, 0o644, "perm bits preserved");
    assert_eq!(a.file_type as u8, fs_ext4_file_type_t::RegFile as u8);

    unsafe { fs_ext4_umount(fs) };

    // Remount RO and confirm persistence.
    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount: {}", last_err_str());
    assert!(path_exists(fs2, "/brand_new.txt"), "survives remount");
    let a2 = stat(fs2, "/brand_new.txt");
    assert_eq!(a2.inode, ino);
    assert_eq!(a2.size, 0);
    unsafe { fs_ext4_umount(fs2) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn create_then_unlink_round_trip() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/ephemeral.txt").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let ino = unsafe { fs_ext4_create(fs, path_c.as_ptr(), 0o600) };
    assert!(ino > 0);
    assert!(path_exists(fs, "/ephemeral.txt"));

    let rc = unsafe { fs_ext4_unlink(fs, path_c.as_ptr()) };
    assert_eq!(rc, 0, "unlink: {}", last_err_str());
    assert!(!path_exists(fs, "/ephemeral.txt"));
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn create_refuses_duplicate_path() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    // /test.txt already exists on ext4-basic.img.
    let path_c = CString::new("/test.txt").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let ino = unsafe { fs_ext4_create(fs, path_c.as_ptr(), 0o644) };
    assert_eq!(ino, 0, "create duplicate must fail");
    let err = last_err_str();
    assert!(err.contains("exist"), "error should mention exists: {err}");
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn create_refuses_missing_parent_directory() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/nope/child.txt").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let ino = unsafe { fs_ext4_create(fs, path_c.as_ptr(), 0o644) };
    assert_eq!(ino, 0);
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn create_refuses_on_ro_mount() {
    let img_c = CString::new(SRC_IMAGE).unwrap();
    let path_c = CString::new("/should_not_appear.txt").unwrap();

    let fs = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount: {}", last_err_str());
    let ino = unsafe { fs_ext4_create(fs, path_c.as_ptr(), 0o644) };
    assert_eq!(ino, 0);
    let err = last_err_str();
    assert!(
        err.contains("read-only") || err.contains("apply_create"),
        "RO error: {err}"
    );
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn create_in_subdir_works() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/subdir/leaf.txt").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let ino = unsafe { fs_ext4_create(fs, path_c.as_ptr(), 0o644) };
    assert!(ino > 0, "subdir create: {}", last_err_str());
    assert!(path_exists(fs, "/subdir/leaf.txt"));
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn create_sets_timestamps_to_now() {
    // Regression: `fs_ext4_create` previously wrote atime/ctime/mtime
    // but left i_crtime at offset 0x90 zero. Finder / `stat -f %B`
    // then showed "1 January 1970" as the birth time. Verify all four
    // timestamp fields land within a tolerance of "now".
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/freshly_minted.txt").unwrap();

    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let ino = unsafe { fs_ext4_create(fs, path_c.as_ptr(), 0o644) };
    assert!(ino > 0, "create returned 0: {}", last_err_str());
    let a = stat(fs, "/freshly_minted.txt");
    unsafe { fs_ext4_umount(fs) };

    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 1;

    for (label, ts) in [
        ("atime", a.atime),
        ("mtime", a.mtime),
        ("ctime", a.ctime),
        ("crtime", a.crtime),
    ] {
        assert!(
            ts >= before && ts <= after,
            "{label}={ts} outside [{before}, {after}]"
        );
    }

    std::fs::remove_file(&img).ok();
}

#[test]
fn create_null_inputs_do_not_crash() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null());

    let path_c = CString::new("/x.txt").unwrap();
    assert_eq!(
        unsafe { fs_ext4_create(std::ptr::null_mut(), path_c.as_ptr(), 0o644) },
        0
    );
    assert_eq!(unsafe { fs_ext4_create(fs, std::ptr::null(), 0o644) }, 0);

    unsafe { fs_ext4_umount(fs) };
    std::fs::remove_file(&img).ok();
}
