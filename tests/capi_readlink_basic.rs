//! Readlink coverage on ext4-basic.img's /link.txt.
//!
//! The fixture recipe (test-disks/guest-build-images.sh) creates it as a
//! symlink to test.txt; this verifies readlink on it works through the C ABI.

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};

const IMAGE: &str = "ext4-basic.img";

fn mount_fixture() -> *mut fs_ext4_fs_t {
    let path = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), IMAGE);
    let p = CString::new(path.as_str()).unwrap();
    let fs = unsafe { fs_ext4_mount(p.as_ptr()) };
    assert!(
        !fs.is_null(),
        "fs_ext4_mount({path}) failed: {}",
        unsafe { std::ffi::CStr::from_ptr(fs_ext4_last_error()) }.to_string_lossy()
    );
    fs
}

#[test]
fn readlink_on_basic_link_returns_expected_target() {
    let fs = mount_fixture();
    let p = CString::new("/link.txt").unwrap();

    // ext4-basic.img's recipe creates /link.txt -> test.txt.
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_ext4_stat(fs, p.as_ptr(), &mut attr) };
    assert_eq!(rc, 0, "/link.txt not present in ext4-basic.img");
    assert!(
        matches!(attr.file_type, fs_ext4_file_type_t::Symlink),
        "/link.txt exists but isn't a symlink (file_type={:?})",
        attr.file_type as u32
    );

    let mut buf = [0u8; 256];
    let rc = unsafe {
        fs_ext4_readlink(
            fs,
            p.as_ptr(),
            buf.as_mut_ptr() as *mut std::ffi::c_char,
            buf.len(),
        )
    };
    assert_eq!(rc, 8, "readlink failed");

    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let target = String::from_utf8_lossy(&buf[..end]);
    assert_eq!(target, "test.txt", "unexpected /link.txt target");

    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn readlink_on_regular_file_sets_einval() {
    let fs = mount_fixture();
    let p = CString::new("/test.txt").unwrap();
    let mut buf = [0u8; 64];
    let rc = unsafe {
        fs_ext4_readlink(
            fs,
            p.as_ptr(),
            buf.as_mut_ptr() as *mut std::ffi::c_char,
            buf.len(),
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22); // EINVAL
    let err = unsafe {
        CStr::from_ptr(fs_ext4_last_error())
            .to_string_lossy()
            .into_owned()
    };
    assert!(err.contains("not a symlink"), "err was: {err}");
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn readlink_on_directory_sets_einval() {
    let fs = mount_fixture();
    let p = CString::new("/subdir").unwrap();
    let mut buf = [0u8; 64];
    let rc = unsafe {
        fs_ext4_readlink(
            fs,
            p.as_ptr(),
            buf.as_mut_ptr() as *mut std::ffi::c_char,
            buf.len(),
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22); // EINVAL
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn readlink_on_missing_path_sets_enoent() {
    let fs = mount_fixture();
    let p = CString::new("/does-not-exist").unwrap();
    let mut buf = [0u8; 64];
    let rc = unsafe {
        fs_ext4_readlink(
            fs,
            p.as_ptr(),
            buf.as_mut_ptr() as *mut std::ffi::c_char,
            buf.len(),
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 2); // ENOENT
    unsafe { fs_ext4_umount(fs) };
}

/// The return value is the target's length, as `readlink(2)` returns it:
/// a caller that slices its buffer by the return value must get the whole
/// target, not an empty string (#290).
#[test]
fn readlink_returns_the_fast_symlink_target_length_and_bytes() {
    let fs = mount_fixture();
    let p = CString::new("/link.txt").unwrap();
    let mut buf = [0xAAu8; 256];
    let rc = unsafe {
        fs_ext4_readlink(
            fs,
            p.as_ptr(),
            buf.as_mut_ptr() as *mut std::ffi::c_char,
            buf.len(),
        )
    };
    assert_eq!(rc, 8, "readlink must return the target length");
    assert_eq!(&buf[..rc as usize], b"test.txt");
    assert_eq!(buf[rc as usize], 0, "target must be NUL-terminated");
    unsafe { fs_ext4_umount(fs) };
}

/// A buffer too small for the target gets as much as fits before the NUL,
/// and the return value counts exactly those bytes.
#[test]
fn readlink_into_a_short_buffer_returns_the_bytes_copied() {
    let fs = mount_fixture();
    let p = CString::new("/link.txt").unwrap();
    let mut buf = [0xAAu8; 4];
    let rc = unsafe {
        fs_ext4_readlink(
            fs,
            p.as_ptr(),
            buf.as_mut_ptr() as *mut std::ffi::c_char,
            buf.len(),
        )
    };
    assert_eq!(rc, 3);
    assert_eq!(&buf, b"tes\0");
    unsafe { fs_ext4_umount(fs) };
}
