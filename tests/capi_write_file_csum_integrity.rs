//! After a write_file or truncate, the resulting on-disk state must still
//! pass metadata-csum verification on a fresh remount. If the write path
//! doesn't update inode/extent/dir checksums, the next mount will reject
//! the very data we just wrote.
//!
//! This test rebuilds a scratch image, writes via the C ABI, unmounts,
//! then re-mounts (which runs all the CSUM verifiers in Filesystem::mount
//! and read_inode_verified). Any mismatch → mount fails or reads fail.

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::Write;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

fn scratch(label: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_wf_csum_{label}_{}_{n}.img",
        std::process::id()
    ));
    let mut out = fs::File::create(&dst).unwrap();
    out.write_all(&fs_ext4_test_support::read_small_file(SRC).unwrap())
        .unwrap();
    dst
}

fn last_err() -> String {
    unsafe {
        CStr::from_ptr(fs_ext4_last_error())
            .to_string_lossy()
            .into_owned()
    }
}

#[test]
fn write_file_result_survives_csum_verification_on_remount() {
    let img = scratch("wf");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    // Write via rw mount.
    {
        let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
        assert!(!fs_h.is_null());
        let payload = b"csum-check replacement payload\n";
        let rc = unsafe {
            fs_ext4_write_file(
                fs_h,
                path_c.as_ptr(),
                payload.as_ptr() as *const c_void,
                payload.len() as u64,
            )
        };
        assert!(rc > 0, "write_file: {}", last_err());
        unsafe { fs_ext4_umount(fs_h) };
    }

    // Remount read-only — this runs Filesystem::mount's csum-seed
    // derivation + verify_superblock + verify_bgd chain, and every stat
    // goes through read_inode_verified. Any CRC mismatch aborts.
    {
        let fs_h = unsafe { fs_ext4_mount(img_c.as_ptr()) };
        assert!(
            !fs_h.is_null(),
            "remount after write_file failed: {}",
            last_err()
        );

        // Stat the file we wrote (read_inode_verified).
        let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
        let rc = unsafe { fs_ext4_stat(fs_h, path_c.as_ptr(), &mut attr) };
        assert_eq!(rc, 0, "stat /test.txt after write: {}", last_err());
        assert_eq!(attr.size, 31);

        // Enumerate root — verifies the root dir block's CRC tail.
        let root = CString::new("/").unwrap();
        let iter = unsafe { fs_ext4_dir_open(fs_h, root.as_ptr()) };
        assert!(!iter.is_null(), "dir_open /: {}", last_err());
        let mut count = 0;
        loop {
            let e = unsafe { fs_ext4_dir_next(iter) };
            if e.is_null() {
                break;
            }
            count += 1;
        }
        unsafe { fs_ext4_dir_close(iter) };
        assert!(count >= 4, "root must still enumerate all entries");

        // Read the file — exercises extent-tail CRC verification.
        let mut buf = [0u8; 64];
        let n = unsafe {
            fs_ext4_read_file(
                fs_h,
                path_c.as_ptr(),
                buf.as_mut_ptr() as *mut c_void,
                0,
                buf.len() as u64,
            )
        };
        assert_eq!(n, 31, "read_file: {}", last_err());
        assert_eq!(&buf[..31], b"csum-check replacement payload\n");

        unsafe { fs_ext4_umount(fs_h) };
    }

    let _ = fs::remove_file(&img);
}

#[test]
fn truncate_result_survives_csum_verification_on_remount() {
    let img = scratch("tr");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    {
        let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
        assert!(!fs_h.is_null());
        let rc = unsafe { fs_ext4_truncate(fs_h, path_c.as_ptr(), 4) };
        assert_eq!(rc, 0, "truncate: {}", last_err());
        unsafe { fs_ext4_umount(fs_h) };
    }

    {
        let fs_h = unsafe { fs_ext4_mount(img_c.as_ptr()) };
        assert!(!fs_h.is_null(), "remount after truncate: {}", last_err());

        let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
        let rc = unsafe { fs_ext4_stat(fs_h, path_c.as_ptr(), &mut attr) };
        assert_eq!(rc, 0, "stat after truncate remount: {}", last_err());
        assert_eq!(attr.size, 4);

        unsafe { fs_ext4_umount(fs_h) };
    }

    let _ = fs::remove_file(&img);
}

#[test]
fn unlink_result_survives_csum_verification_on_remount() {
    let img = scratch("ul");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    {
        let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
        assert!(!fs_h.is_null());
        let rc = unsafe { fs_ext4_unlink(fs_h, path_c.as_ptr()) };
        assert_eq!(rc, 0, "unlink: {}", last_err());
        unsafe { fs_ext4_umount(fs_h) };
    }

    {
        let fs_h = unsafe { fs_ext4_mount(img_c.as_ptr()) };
        assert!(!fs_h.is_null(), "remount after unlink: {}", last_err());

        // File should be gone.
        let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
        let rc = unsafe { fs_ext4_stat(fs_h, path_c.as_ptr(), &mut attr) };
        assert_eq!(rc, -1, "stat of unlinked file should fail");
        assert_eq!(fs_ext4_last_errno(), 2, "ENOENT expected");

        // Root should still enumerate (without the unlinked entry).
        let root = CString::new("/").unwrap();
        let iter = unsafe { fs_ext4_dir_open(fs_h, root.as_ptr()) };
        assert!(!iter.is_null(), "dir_open /: {}", last_err());
        let mut names = Vec::new();
        loop {
            let e = unsafe { fs_ext4_dir_next(iter) };
            if e.is_null() {
                break;
            }
            let ent = unsafe { &*e };
            let bytes: Vec<u8> = ent.name[..ent.name_len as usize]
                .iter()
                .map(|b| b.to_ne_bytes()[0])
                .collect();
            names.push(String::from_utf8_lossy(&bytes).into_owned());
        }
        unsafe { fs_ext4_dir_close(iter) };
        assert!(
            !names.contains(&"test.txt".to_string()),
            "test.txt should be absent"
        );

        unsafe { fs_ext4_umount(fs_h) };
    }

    let _ = fs::remove_file(&img);
}
