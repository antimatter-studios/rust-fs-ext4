//! Integration tests for `fs_ext4_symlink` (fast-symlink path).

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
        "fs_ext4_capi_symlink_{tag}_{}_{n}.img",
        std::process::id()
    ));
    let bytes = fs::read(SRC).expect("read src");
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
fn symlink_creates_link_with_target() {
    let img = scratch("basic");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let target_c = CString::new("/etc/hosts").unwrap();
    let link_c = CString::new("/mylink").unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let ino = unsafe { fs_ext4_symlink(fs_h, target_c.as_ptr(), link_c.as_ptr()) };
    assert!(ino > 0, "symlink returned {ino}");
    assert_eq!(fs_ext4_last_errno(), 0);

    // stat → file_type = Symlink, size = target len.
    let attr = stat_attr(fs_h, "/mylink");
    assert!(matches!(attr.file_type, fs_ext4_file_type_t::Symlink));
    assert_eq!(attr.size, "/etc/hosts".len() as u64);
    assert_eq!(attr.inode, ino);

    // readlink writes the target NUL-terminated into buf; returns its length.
    let mut buf = [0u8; 256];
    let rc =
        unsafe { fs_ext4_readlink(fs_h, link_c.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len()) };
    let nul = buf.iter().position(|&b| b == 0).expect("NUL terminator");
    assert_eq!(rc as usize, nul, "readlink returned {rc}");
    assert_eq!(&buf[..nul], b"/etc/hosts");

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn symlink_survives_remount_with_csum() {
    let img = scratch("remount");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let target_c = CString::new("../relative/path").unwrap();
    let link_c = CString::new("/reloc").unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let ino = unsafe { fs_ext4_symlink(fs_h, target_c.as_ptr(), link_c.as_ptr()) };
    assert!(ino > 0);
    unsafe { fs_ext4_umount(fs_h) };

    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount failed — inode csum not patched?");
    let attr = stat_attr(fs2, "/reloc");
    assert!(matches!(attr.file_type, fs_ext4_file_type_t::Symlink));
    assert_eq!(attr.size, "../relative/path".len() as u64);

    let mut buf = [0u8; 64];
    let rc =
        unsafe { fs_ext4_readlink(fs2, link_c.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len()) };
    let nul = buf.iter().position(|&b| b == 0).expect("NUL terminator");
    assert_eq!(rc as usize, nul);
    assert_eq!(&buf[..nul], b"../relative/path");
    unsafe { fs_ext4_umount(fs2) };
    let _ = fs::remove_file(&img);
}

#[test]
fn symlink_existing_path_returns_eexist() {
    let img = scratch("exist");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let target_c = CString::new("whatever").unwrap();
    let link_c = CString::new("/test.txt").unwrap(); // pre-existing file

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let ino = unsafe { fs_ext4_symlink(fs_h, target_c.as_ptr(), link_c.as_ptr()) };
    assert_eq!(ino, 0);
    assert_eq!(fs_ext4_last_errno(), 17, "EEXIST expected");

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn symlink_slow_path_target_over_60_bytes_roundtrips() {
    // Targets 61..=255 bytes go through the slow path: 1 block allocated,
    // target written there, extent inserted into the inode.
    let long_target = "/".to_string() + &"long_path_component/".repeat(8) + "leaf"; // ~164 bytes
    assert!(long_target.len() > 60);
    assert!(long_target.len() <= 255);

    let img = scratch("slow");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let target_c = CString::new(long_target.as_str()).unwrap();
    let link_c = CString::new("/slowlink").unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let ino = unsafe { fs_ext4_symlink(fs_h, target_c.as_ptr(), link_c.as_ptr()) };
    assert!(ino > 0, "slow symlink creation failed");
    assert_eq!(fs_ext4_last_errno(), 0);

    let attr = stat_attr(fs_h, "/slowlink");
    assert!(matches!(attr.file_type, fs_ext4_file_type_t::Symlink));
    assert_eq!(attr.size, long_target.len() as u64);

    let mut buf = [0u8; 512];
    let rc =
        unsafe { fs_ext4_readlink(fs_h, link_c.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len()) };
    let nul = buf.iter().position(|&b| b == 0).expect("NUL terminator");
    assert_eq!(rc as usize, nul);
    assert_eq!(&buf[..nul], long_target.as_bytes());

    unsafe { fs_ext4_umount(fs_h) };

    // Persists across csum-validated remount.
    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(
        !fs2.is_null(),
        "remount failed — inode/extent csum not patched?"
    );
    let mut buf = [0u8; 512];
    let rc =
        unsafe { fs_ext4_readlink(fs2, link_c.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len()) };
    let nul = buf.iter().position(|&b| b == 0).expect("NUL terminator");
    assert_eq!(rc as usize, nul);
    assert_eq!(&buf[..nul], long_target.as_bytes());
    unsafe { fs_ext4_umount(fs2) };

    let _ = fs::remove_file(&img);
}

#[test]
fn symlink_target_over_path_max_returns_enametoolong() {
    // 5000 bytes — over PATH_MAX (4096) and over the FFI cap. ext4 stores the
    // slow target inline in one fs block, so anything past block_size is
    // refused.
    let long_target = "x".repeat(5000);
    let img = scratch("toolong");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let target_c = CString::new(long_target.clone()).unwrap();
    let link_c = CString::new("/toolong").unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let ino = unsafe { fs_ext4_symlink(fs_h, target_c.as_ptr(), link_c.as_ptr()) };
    assert_eq!(ino, 0);
    assert_eq!(
        fs_ext4_last_errno(),
        63,
        "ENAMETOOLONG expected (got {})",
        fs_ext4_last_errno()
    );
    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn symlink_empty_target_returns_einval() {
    let img = scratch("empty");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let target_c = CString::new("").unwrap();
    let link_c = CString::new("/emptytarget").unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let ino = unsafe { fs_ext4_symlink(fs_h, target_c.as_ptr(), link_c.as_ptr()) };
    assert_eq!(ino, 0);
    assert_eq!(fs_ext4_last_errno(), 22);
    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn symlink_null_args_return_einval() {
    let img = scratch("null");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let t = CString::new("x").unwrap();
    let ino = unsafe { fs_ext4_symlink(fs_h, std::ptr::null(), t.as_ptr()) };
    assert_eq!(ino, 0);
    assert_eq!(fs_ext4_last_errno(), 22);
    let ino = unsafe { fs_ext4_symlink(fs_h, t.as_ptr(), std::ptr::null()) };
    assert_eq!(ino, 0);
    assert_eq!(fs_ext4_last_errno(), 22);
    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}
