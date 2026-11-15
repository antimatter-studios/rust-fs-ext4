//! Phase 4 truncate against a file with a deep extent tree.
//!
//! ext4-deep-extents.img has a sparse file with a multi-level extent tree.
//! Both deep and inline trees support truncation through the same C ABI.

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

#[track_caller]
fn scratch() -> PathBuf {
    let src = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), "ext4-deep-extents.img");
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_truncate_deep_{}_{n}.img",
        std::process::id()
    ));
    let bytes = fs_ext4_test_support::read_small_file(&src).expect("read src");
    let mut out = fs::File::create(&dst).expect("create");
    out.write_all(&bytes).expect("write");
    out.flush().expect("flush");
    dst
}

fn last_err() -> String {
    unsafe {
        let p = fs_ext4_last_error();
        if p.is_null() {
            return String::new();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

#[test]
fn truncate_on_multi_level_extent_tree_succeeds() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/sparse.bin").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err());

    // Try to shrink the 16 MiB sparse file to 64 KiB.
    let rc = unsafe { fs_ext4_truncate(fs, path_c.as_ptr(), 65_536) };
    assert_eq!(rc, 0, "multi-level truncate: {}", last_err());

    // Dense (inline-extent) file in the same image should be truncatable —
    // also exercises both layouts on the same writable mount.
    let dense = CString::new("/dense.txt").unwrap();
    let rc2 = unsafe { fs_ext4_truncate(fs, dense.as_ptr(), 4) };
    assert_eq!(rc2, 0, "single-extent truncate: {}", last_err());

    unsafe { fs_ext4_umount(fs) };
    let _ = fs::remove_file(&img);
}

#[test]
fn truncate_on_single_extent_file_still_works() {
    // Same idea as above but isolated, in case someone removes the dense
    // fallback from the first test: verify the straightforward case stays
    // healthy on this image.
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err());
    let dense = CString::new("/dense.txt").unwrap();
    let rc = unsafe { fs_ext4_truncate(fs, dense.as_ptr(), 0) };
    assert_eq!(rc, 0, "dense truncate: {}", last_err());
    unsafe { fs_ext4_umount(fs) };
    let _ = fs::remove_file(&img);
}
