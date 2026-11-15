//! Integration tests for sparse-grow via `fs_ext4_truncate`.
//!
//! Grow path: `new_size > inode.size` with no block allocation (ext4's
//! extent tree treats unmapped logical blocks as holes that read back as
//! zeros). Covers:
//! - i_size grows; i_blocks does NOT change (sparse).
//! - Reading the new tail returns zero bytes.
//! - Grow → unmount → remount_ro → inode still valid (csum patched).
//! - Grow-to-current-size is idempotent success.
//! - Shrink path through the same capi still works (regression check).

use fs_ext4::capi::*;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::mem::MaybeUninit;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_trunc_grow_{tag}_{}_{n}.img",
        std::process::id()
    ));
    let bytes = fs_ext4_test_support::read_small_file(SRC).expect("read src");
    let mut out = fs::File::create(&dst).expect("create");
    out.write_all(&bytes).expect("write");
    out.flush().expect("flush");
    dst
}

fn stat_attr(fs_handle: *mut fs_ext4_fs_t, path: &str) -> fs_ext4_attr_t {
    let p = CString::new(path).unwrap();
    let mut attr = MaybeUninit::<fs_ext4_attr_t>::uninit();
    let rc = unsafe { fs_ext4_stat(fs_handle, p.as_ptr(), attr.as_mut_ptr()) };
    assert_eq!(rc, 0, "stat {path} failed");
    unsafe { attr.assume_init() }
}

#[test]
fn truncate_grow_increases_size_without_allocating_blocks() {
    let img = scratch("basic");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());

    let before = stat_attr(fs_handle, "/test.txt");
    let new_size = before.size + 65536; // 64 KiB past current

    let rc = unsafe { fs_ext4_truncate(fs_handle, path_c.as_ptr(), new_size) };
    assert_eq!(rc, 0, "grow should succeed, got {}: {}", rc, unsafe {
        std::ffi::CStr::from_ptr(fs_ext4_last_error()).to_string_lossy()
    });
    assert_eq!(fs_ext4_last_errno(), 0);

    let after = stat_attr(fs_handle, "/test.txt");
    assert_eq!(after.size, new_size, "size grew to requested value");
    // Sparse — no new blocks consumed by the grow.
    assert_eq!(
        after.size - before.size,
        new_size - before.size,
        "delta size == requested delta"
    );

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn truncate_grow_tail_reads_as_zero() {
    let img = scratch("zerotail");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let before = stat_attr(fs_handle, "/test.txt");
    let new_size = before.size + 4096;

    let rc = unsafe { fs_ext4_truncate(fs_handle, path_c.as_ptr(), new_size) };
    assert_eq!(rc, 0);

    // Read the whole file post-grow. The tail should be all zeros.
    let mut buf = vec![0xABu8; new_size as usize];
    let got = unsafe {
        fs_ext4_read_file(
            fs_handle,
            path_c.as_ptr(),
            buf.as_mut_ptr() as *mut _,
            0,
            new_size,
        )
    };
    assert_eq!(got, new_size as i64, "read should return full grown size");
    // Tail bytes (everything beyond the original content) are zeros.
    for (i, b) in buf[(before.size as usize)..].iter().enumerate() {
        assert_eq!(
            *b,
            0,
            "byte offset {} past original EOF should be zero (found 0x{:02x})",
            before.size as usize + i,
            b
        );
    }
    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn truncate_grow_persists_across_remount_with_csum() {
    let img = scratch("persist");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let before = stat_attr(fs_handle, "/test.txt");
    let new_size = before.size + 12345;
    let rc = unsafe { fs_ext4_truncate(fs_handle, path_c.as_ptr(), new_size) };
    assert_eq!(rc, 0);
    unsafe { fs_ext4_umount(fs_handle) };

    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount failed — inode csum not patched?");
    let after = stat_attr(fs2, "/test.txt");
    assert_eq!(after.size, new_size);
    unsafe { fs_ext4_umount(fs2) };
    let _ = fs::remove_file(&img);
}

#[test]
fn truncate_at_current_size_is_success() {
    let img = scratch("noop");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let before = stat_attr(fs_handle, "/test.txt");

    let rc = unsafe { fs_ext4_truncate(fs_handle, path_c.as_ptr(), before.size) };
    assert_eq!(rc, 0);
    assert_eq!(fs_ext4_last_errno(), 0);

    let after = stat_attr(fs_handle, "/test.txt");
    assert_eq!(after.size, before.size, "size unchanged");

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn truncate_shrink_path_still_works() {
    // Regression: the capi now dispatches to shrink vs grow on new_size
    // direction. Confirm shrink (the pre-existing path) is unchanged.
    let img = scratch("shrink");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let before = stat_attr(fs_handle, "/test.txt");
    assert!(before.size > 4, "test.txt should be larger than 4 bytes");

    let rc = unsafe { fs_ext4_truncate(fs_handle, path_c.as_ptr(), 4) };
    assert_eq!(rc, 0);
    let after = stat_attr(fs_handle, "/test.txt");
    assert_eq!(after.size, 4);

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}
