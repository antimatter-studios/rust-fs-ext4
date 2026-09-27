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

fn readlink_into(fs: *mut fs_ext4_fs_t, path: &str, buf: &mut [u8]) -> i32 {
    let p = CString::new(path).unwrap();
    unsafe {
        fs_ext4_readlink(
            fs,
            p.as_ptr(),
            buf.as_mut_ptr() as *mut std::ffi::c_char,
            buf.len(),
        )
    }
}

/// A buffer with room for the target but not its NUL is refused with
/// ERANGE, and nothing is written: never a silent truncation.
#[test]
fn readlink_one_byte_short_for_the_nul_is_erange_and_writes_nothing() {
    let fs = mount_fixture();
    let mut buf = [0xAAu8; 8]; // "test.txt" is 8 bytes; the NUL needs a 9th
    let rc = readlink_into(fs, "/link.txt", &mut buf);
    assert_eq!(rc, -1, "a truncated target must not be returned");
    assert_eq!(fs_ext4_last_errno(), 34, "errno must be ERANGE");
    assert_eq!(buf, [0xAAu8; 8], "buf must be untouched");
    let err = unsafe { CStr::from_ptr(fs_ext4_last_error()) }
        .to_string_lossy()
        .into_owned();
    assert!(
        err.contains('9'),
        "the message must name the size needed: {err}"
    );
    unsafe { fs_ext4_umount(fs) };
}

/// Any smaller buffer is refused the same way.
#[test]
fn readlink_into_a_short_buffer_is_erange_and_writes_nothing() {
    let fs = mount_fixture();
    let mut buf = [0xAAu8; 4];
    let rc = readlink_into(fs, "/link.txt", &mut buf);
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 34);
    assert_eq!(buf, [0xAAu8; 4]);
    unsafe { fs_ext4_umount(fs) };
}

/// A non-NULL buffer of size 0 is only too small: ERANGE, not EINVAL.
#[test]
fn readlink_with_bufsize_zero_is_erange() {
    let fs = mount_fixture();
    let p = CString::new("/link.txt").unwrap();
    let mut byte = 0xAAu8;
    let rc = unsafe { fs_ext4_readlink(fs, p.as_ptr(), (&mut byte as *mut u8).cast(), 0) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 34, "errno must be ERANGE");
    assert_eq!(byte, 0xAA, "nothing may be written");
    let err = unsafe { CStr::from_ptr(fs_ext4_last_error()) }
        .to_string_lossy()
        .into_owned();
    assert!(
        err.contains('9'),
        "the message must name the size needed: {err}"
    );
    unsafe { fs_ext4_umount(fs) };
}

/// Target plus NUL exactly fills the buffer: success.
#[test]
fn readlink_into_an_exact_fit_buffer_succeeds() {
    let fs = mount_fixture();
    let mut buf = [0xAAu8; 9];
    let rc = readlink_into(fs, "/link.txt", &mut buf);
    assert_eq!(rc, 8);
    assert_eq!(&buf, b"test.txt\0");
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn readlink_with_a_null_buffer_is_einval() {
    let fs = mount_fixture();
    let p = CString::new("/link.txt").unwrap();
    let rc = unsafe { fs_ext4_readlink(fs, p.as_ptr(), std::ptr::null_mut(), 64) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22);
    unsafe { fs_ext4_umount(fs) };
}

/// A symlink declaring a target longer than any path fails with errno
/// set, not only a message.
#[test]
fn readlink_of_an_oversize_target_sets_errno() {
    use fs_ext4::block_io::{BlockDevice, FileDevice};
    use fs_ext4::fs::Filesystem;
    use std::sync::Arc;

    let src = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), IMAGE);
    let img =
        fs_ext4_test_support::temp_path!("fs_ext4_readlink_oversize_{}.img", std::process::id());
    std::fs::copy(&src, &img).expect("copy fixture");
    {
        let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open_rw(&img).expect("open_rw"));
        let fs = Filesystem::mount(dev.clone()).expect("mount");
        let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(inode, _)| inode);
        let ino =
            fs_ext4::path::lookup(dev.as_ref(), &fs.sb, &mut reader, "/link.txt").expect("lookup");
        let (inode, mut raw) = fs.read_inode_verified(ino).expect("inode");
        raw[0x6C..0x70].copy_from_slice(&1u32.to_le_bytes()); // i_size_high: 4 GiB + 8
        if let Some((lo, hi)) = fs.csum.compute_inode_checksum(ino, inode.generation, &raw) {
            raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
            raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
        }
        fs.write_inode_raw(ino, &raw).expect("write inode");
        dev.flush().expect("flush");
    }
    let c = CString::new(img.as_str()).unwrap();
    let fs = unsafe { fs_ext4_mount(c.as_ptr()) };
    assert!(!fs.is_null());
    let mut buf = [0xAAu8; 64];
    let rc = readlink_into(fs, "/link.txt", &mut buf);
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 5, "errno must be set (EIO)");
    let err = unsafe { CStr::from_ptr(fs_ext4_last_error()) }
        .to_string_lossy()
        .into_owned();
    assert!(err.contains("longer than any path"), "err was: {err}");
    assert_eq!(buf, [0xAAu8; 64]);
    unsafe { fs_ext4_umount(fs) };
    std::fs::remove_file(&img).ok();
}
