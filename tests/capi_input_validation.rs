//! Tests for the FFI input-validation hardening landed under Phase 7.3.
//!
//! Three guards under test:
//!   1. **Path length cap** — `cstr_to_str` returns `""` for any path
//!      longer than `FFI_PATH_MAX` (4096) so downstream lookups land at
//!      a clearly-invalid empty path instead of walking a multi-megabyte
//!      buffer twice.
//!   2. **Write-path length cap** — `fs_ext4_write_file` and
//!      `fs_ext4_setxattr` reject `len`/`value_len` exceeding their
//!      hard ceilings before constructing a `&[u8]` from raw parts
//!      (which would be UB even before any read happens).
//!   3. **Read-path length clamping** — `fs_ext4_read_file` clamps an
//!      oversize `length` against the file's actual size so a caller
//!      passing `u64::MAX` doesn't fabricate an absurd output slice.
//!
//! All three are FFI-safety guards — they protect against hostile or
//! buggy callers from outside Rust. Tests pass `path` / `len` directly
//! through the C ABI (via the rlib export pattern the existing capi
//! tests use; see tests/capi_basic.rs preamble).

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};

const TEST_IMAGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

fn last_err_str() -> String {
    unsafe {
        let p = fs_ext4_last_error();
        if p.is_null() {
            return String::new();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

fn mount_test_image() -> *mut fs_ext4_fs_t {
    let path = CString::new(TEST_IMAGE).unwrap();
    let fs = unsafe { fs_ext4_mount(path.as_ptr()) };
    assert!(!fs.is_null(), "mount: {}", last_err_str());
    fs
}

#[test]
fn write_file_rejects_oversize_len() {
    let fs = mount_test_image();
    // Read-only mount — the rejection should fire BEFORE the RO check
    // since the length cap is structural input validation, not a permission
    // check. (Both produce -1; we only assert on the message + errno.)
    let path = CString::new("/anything").unwrap();
    let dummy_data = [0u8; 1];
    // 2 GiB is one byte over our 1 GiB cap; ensures we hit the guard.
    let oversize_len: u64 = (1u64 << 30) + 1;
    let n = unsafe {
        fs_ext4_write_file(
            fs,
            path.as_ptr(),
            dummy_data.as_ptr() as *const c_void,
            oversize_len,
        )
    };
    assert_eq!(n, -1, "expected rejection of oversize write_file len");
    let msg = last_err_str();
    assert!(
        msg.contains("len") && msg.contains("exceeds"),
        "expected len-cap message, got: {msg}"
    );
    assert_eq!(fs_ext4_last_errno(), 22 /* EINVAL */);
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn setxattr_rejects_oversize_value_len() {
    let fs = mount_test_image();
    let path = CString::new("/anything").unwrap();
    let name = CString::new("user.test").unwrap();
    let dummy_value = [0u8; 1];
    // 128 KiB is past our 64 KiB cap; ensures we hit the guard before
    // any path resolution / RO check.
    let oversize_len: usize = 128 * 1024;
    let rc = unsafe {
        fs_ext4_setxattr(
            fs,
            path.as_ptr(),
            name.as_ptr(),
            dummy_value.as_ptr() as *const c_void,
            oversize_len,
        )
    };
    assert_eq!(rc, -1, "expected rejection of oversize setxattr value_len");
    let msg = last_err_str();
    assert!(
        msg.contains("value_len") && msg.contains("exceeds"),
        "expected value_len cap message, got: {msg}"
    );
    assert_eq!(fs_ext4_last_errno(), 22 /* EINVAL */);
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn read_file_clamps_oversize_length_to_file_size() {
    // Read-clamp is checked structurally: we ask for u64::MAX bytes from
    // a known file; the cap forces the slice to be at most file_size,
    // which is small. The read should succeed with at-most-file_size
    // bytes returned (not crash, not return a wildly oversized slice).
    let fs = mount_test_image();
    // First find any regular file in root via dir_open.
    let root = CString::new("/").unwrap();
    let dir = unsafe { fs_ext4_dir_open(fs, root.as_ptr()) };
    assert!(!dir.is_null(), "dir_open(/): {}", last_err_str());
    let mut chosen_name: Option<String> = None;
    loop {
        let dent = unsafe { fs_ext4_dir_next(dir) };
        if dent.is_null() {
            break;
        }
        let d = unsafe { &*dent };
        // file_type is a u8 (POSIX-style ext4 dirent file_type byte) on
        // the dirent struct; RegFile is value 1 in fs_ext4_file_type_t.
        if d.file_type == fs_ext4_file_type_t::RegFile as u8 {
            // d.name is a fixed-size [c_char; 256] zero-terminated.
            let name_ptr: *const c_char = d.name.as_ptr();
            let name = unsafe { CStr::from_ptr(name_ptr) }
                .to_string_lossy()
                .into_owned();
            chosen_name = Some(format!("/{name}"));
            break;
        }
    }
    unsafe { fs_ext4_dir_close(dir) };
    let path_str = chosen_name.expect("test image must contain at least one regular file");
    let path = CString::new(path_str.clone()).unwrap();

    let mut buf = vec![0u8; 64 * 1024]; // big enough for any small fixture file
    let n = unsafe {
        fs_ext4_read_file(
            fs,
            path.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            0,
            u64::MAX, // hostile length — must be clamped
        )
    };
    assert!(
        n >= 0,
        "read_file with u64::MAX length must clamp + succeed, got n={n} err={}",
        last_err_str()
    );
    assert!(
        (n as usize) <= buf.len(),
        "clamped read returned more bytes ({n}) than the host buffer ({})",
        buf.len()
    );
    unsafe { fs_ext4_umount(fs) };
}

/// A path longer than `FFI_PATH_MAX` (4096, Linux's `PATH_MAX`) is refused
/// with ENAMETOOLONG before it is walked — and is never the root.
///
/// THIS TEST USED TO ASSERT THE OPPOSITE, and said so: that such a path
/// "MUST return successfully AND attr.inode == EXT4_ROOT_INODE (2)",
/// pinning the cap's old implementation — `cstr_to_str` returned `""` past
/// the cap, and `""` is a deliberate spelling of the root here. That was
/// the #418 defect itself: a path this driver could not use was answered
/// as a successful stat of the root, indistinguishable from a real hit.
/// The hardening it meant to pin — bounded work on a hostile length — is
/// kept; only the answer changed, from "the root" to the refusal.
#[test]
fn a_path_longer_than_path_max_is_enametoolong_and_never_the_root() {
    let fs = mount_test_image();
    let long_path: String = std::iter::once('/')
        .chain(std::iter::repeat_n('a', 5000))
        .collect();
    let path = CString::new(long_path).unwrap();
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_ext4_stat(fs, path.as_ptr(), &mut attr) };
    assert_eq!(
        rc, -1,
        "a 5001-byte path was answered as a stat of inode {} (mode {:o})",
        attr.inode, attr.mode
    );
    assert_eq!(
        fs_ext4_last_errno(),
        63, // ENAMETOOLONG
        "a path over PATH_MAX must be ENAMETOOLONG: {}",
        last_err_str()
    );
    assert_eq!(attr.mode, 0, "the refusal wrote the root's attributes");
    unsafe { fs_ext4_umount(fs) };
}
