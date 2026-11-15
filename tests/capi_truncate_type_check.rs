//! Truncate type-check regression.
//!
//! SERIOUS BUG found against @3 fs_ext4_truncate: truncate(/dir, 0)
//! silently succeeds, zeros the directory's size, and frees its data
//! blocks — leaving the filesystem in a corrupted state where the dir
//! still exists as an inode but has lost all entries (including . and
//! ..). Finder calling truncate via an FSKit op could therefore destroy
//! a directory's entire subtree without error.
//!
//! POSIX ftruncate(2): "If fildes refers to a directory, ftruncate()
//! shall fail with EISDIR."
//!
//! Current capi.rs calls resolve_path → apply_truncate_shrink without
//! checking `inode.is_file()` first. The fix is to gate on file type
//! before calling apply_truncate_shrink, mirroring the guard in
//! fs_ext4_read_file (which returns EINVAL for non-files).

use fs_ext4::capi::*;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

fn scratch() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_trunc_type_{}_{n}.img",
        std::process::id()
    ));
    let mut out = fs::File::create(&dst).unwrap();
    out.write_all(&fs_ext4_test_support::read_small_file(SRC).unwrap())
        .unwrap();
    dst
}

#[test]
fn truncate_on_directory_should_fail_with_eisdir() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let dir = CString::new("/subdir").unwrap();
    let rc = unsafe { fs_ext4_truncate(fs_h, dir.as_ptr(), 0) };
    assert_eq!(rc, -1, "truncate on directory MUST fail");
    // POSIX EISDIR = 21 on Linux/macOS. Also accept EINVAL (22) as our
    // general type-mismatch code.
    let e = fs_ext4_last_errno();
    assert!(
        e == 21 || e == 22,
        "expected EISDIR (21) or EINVAL (22), got {e}"
    );

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn truncate_grow_succeeds_via_sparse_path() {
    // Grow is now supported via the sparse path: i_size grows, no block
    // allocation, reads of the new tail return zeros. Kept as a type-check
    // test so future changes to truncate semantics surface here.
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let path = CString::new("/test.txt").unwrap();
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    unsafe { fs_ext4_stat(fs_h, path.as_ptr(), &mut attr) };
    let original = attr.size;

    let rc = unsafe { fs_ext4_truncate(fs_h, path.as_ptr(), original + 4096) };
    assert_eq!(rc, 0, "grow now succeeds via sparse path");
    assert_eq!(fs_ext4_last_errno(), 0);

    let mut after: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    unsafe { fs_ext4_stat(fs_h, path.as_ptr(), &mut after) };
    assert_eq!(after.size, original + 4096);

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn truncate_on_symlink_fails_with_einval() {
    // link.txt is a symlink (→ test.txt). Truncating a symlink also isn't
    // allowed per POSIX; our guard returns EINVAL for any non-file non-dir.
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let link = CString::new("/link.txt").unwrap();
    let rc = unsafe { fs_ext4_truncate(fs_h, link.as_ptr(), 0) };
    assert_eq!(rc, -1, "truncate on symlink MUST fail");
    assert_eq!(fs_ext4_last_errno(), 22, "expected EINVAL for symlink");

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn truncate_on_directory_leaves_the_dir_intact() {
    // Regression for the corruption bug: confirm that after a rejected
    // truncate the directory is still enumerable (still has . and ..).
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let dir = CString::new("/subdir").unwrap();
    let _ = unsafe { fs_ext4_truncate(fs_h, dir.as_ptr(), 0) };

    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    unsafe { fs_ext4_stat(fs_h, dir.as_ptr(), &mut attr) };
    assert!(attr.size > 0, "directory size must not be zeroed");

    let iter = unsafe { fs_ext4_dir_open(fs_h, dir.as_ptr()) };
    assert!(!iter.is_null());
    let mut count = 0;
    loop {
        let e = unsafe { fs_ext4_dir_next(iter) };
        if e.is_null() {
            break;
        }
        count += 1;
    }
    unsafe { fs_ext4_dir_close(iter) };
    assert!(count >= 2, "directory must still contain at least . and ..");

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}
