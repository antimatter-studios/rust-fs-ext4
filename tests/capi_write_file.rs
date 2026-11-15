//! C-ABI tests for `fs_ext4_write_file` — the "save-as" path: replace
//! an existing file's body with new bytes. Pairs naturally with
//! `fs_ext4_create` (create + write).

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::io::Write;
use std::os::raw::c_void;
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
        "fs_ext4_capi_write_file_{}_{n}.img",
        std::process::id()
    ));
    let bytes = fs_ext4_test_support::read_small_file(SRC_IMAGE).expect("read src image");
    let mut out = std::fs::File::create(&dst).expect("create dst image");
    out.write_all(&bytes).expect("write dst image");
    out.flush().expect("flush");
    drop(out);
    dst
}

fn read_all(fs: *mut fs_ext4_fs_t, path: &str, stat_size: u64) -> Vec<u8> {
    let p = CString::new(path).unwrap();
    let mut buf = vec![0u8; stat_size as usize];
    let n = unsafe {
        fs_ext4_read_file(
            fs,
            p.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            0,
            buf.len() as u64,
        )
    };
    assert!(n >= 0, "read {path}: {}", last_err_str());
    buf.truncate(n as usize);
    buf
}

fn stat_size(fs: *mut fs_ext4_fs_t, path: &str) -> u64 {
    let p = CString::new(path).unwrap();
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_ext4_stat(fs, p.as_ptr(), &mut attr as *mut _) };
    assert_eq!(rc, 0, "stat {path}: {}", last_err_str());
    attr.size
}

fn write_bytes(fs: *mut fs_ext4_fs_t, path: &str, data: &[u8]) -> i64 {
    let p = CString::new(path).unwrap();
    unsafe {
        fs_ext4_write_file(
            fs,
            p.as_ptr(),
            data.as_ptr() as *const c_void,
            data.len() as u64,
        )
    }
}

#[test]
fn create_then_write_then_read_round_trip() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path = "/greeting.txt";
    let path_c = CString::new(path).unwrap();
    let payload = b"hello from ext4rs write_file!\n";

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let ino = unsafe { fs_ext4_create(fs, path_c.as_ptr(), 0o644) };
    assert!(ino > 0, "create: {}", last_err_str());

    let n = write_bytes(fs, path, payload);
    assert_eq!(n, payload.len() as i64, "write_file: {}", last_err_str());
    assert_eq!(stat_size(fs, path), payload.len() as u64);

    let content = read_all(fs, path, stat_size(fs, path));
    assert_eq!(content, payload);

    unsafe { fs_ext4_umount(fs) };

    // Persist across remount.
    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount: {}", last_err_str());
    assert_eq!(stat_size(fs2, path), payload.len() as u64);
    let content2 = read_all(fs2, path, stat_size(fs2, path));
    assert_eq!(content2, payload);
    unsafe { fs_ext4_umount(fs2) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn overwrite_existing_file_shrinks_and_swaps_content() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    // /test.txt already has some content on ext4-basic.img.
    let path = "/test.txt";

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let original_size = stat_size(fs, path);
    assert!(original_size > 0);

    let new_payload = b"overwritten\n";
    let n = write_bytes(fs, path, new_payload);
    assert_eq!(n, new_payload.len() as i64, "write: {}", last_err_str());
    assert_eq!(stat_size(fs, path), new_payload.len() as u64);

    let content = read_all(fs, path, stat_size(fs, path));
    assert_eq!(content, new_payload);
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn write_empty_clears_file_content() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path = "/test.txt";
    let path_c = CString::new(path).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    assert!(stat_size(fs, path) > 0);

    let n = unsafe { fs_ext4_write_file(fs, path_c.as_ptr(), std::ptr::null(), 0) };
    assert_eq!(n, 0, "empty write: {}", last_err_str());
    assert_eq!(stat_size(fs, path), 0);
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn write_multiblock_payload_allocates_contiguous_run() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path = "/large.bin";
    let path_c = CString::new(path).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let ino = unsafe { fs_ext4_create(fs, path_c.as_ptr(), 0o644) };
    assert!(ino > 0, "create: {}", last_err_str());

    // 8 KiB — exceeds one 4K block or 1K block, forcing a multi-block
    // allocation regardless of ext4-basic.img's block size.
    let payload: Vec<u8> = (0..8192).map(|i| (i & 0xFF) as u8).collect();
    let n = write_bytes(fs, path, &payload);
    assert_eq!(n, payload.len() as i64, "write: {}", last_err_str());
    let content = read_all(fs, path, stat_size(fs, path));
    assert_eq!(content, payload, "round-trip content mismatch");
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn write_to_nonexistent_path_returns_minus_one() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/does-not-exist.txt").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let data = b"doesn't matter";
    let n = unsafe {
        fs_ext4_write_file(
            fs,
            path_c.as_ptr(),
            data.as_ptr() as *const c_void,
            data.len() as u64,
        )
    };
    assert_eq!(n, -1);
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn write_on_ro_mount_returns_minus_one() {
    let img_c = CString::new(SRC_IMAGE).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount: {}", last_err_str());
    let data = b"no";
    let n = unsafe {
        fs_ext4_write_file(
            fs,
            path_c.as_ptr(),
            data.as_ptr() as *const c_void,
            data.len() as u64,
        )
    };
    assert_eq!(n, -1);
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn write_rejects_null_data_with_nonzero_len() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let n = unsafe { fs_ext4_write_file(fs, path_c.as_ptr(), std::ptr::null(), 42) };
    assert_eq!(n, -1);
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}
