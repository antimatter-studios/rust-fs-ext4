//! End-to-end write-path exercise: chain multiple mutations in one mount
//! then verify the result survives a fresh mount's csum chain.
//!
//! Catches subtle state bugs that single-op tests miss (e.g. state left
//! in a mid-mutation invariant-violated state that subsequent ops rely
//! on, but the on-disk form still passes verification because the final
//! mutation happens to patch it up).

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::Write;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

fn scratch(label: &str) -> PathBuf {
    static C: AtomicU32 = AtomicU32::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_e2e_{label}_{}_{n}.img",
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
fn create_write_rename_read_truncate_unlink_chain() {
    let img = scratch("chain");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let p_initial = CString::new("/chain.txt").unwrap();
    let p_renamed = CString::new("/subdir/chain_moved.txt").unwrap();
    let payload = b"chain-of-ops payload v1";

    {
        let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
        assert!(!fs_h.is_null());

        // create
        let ino = unsafe { fs_ext4_create(fs_h, p_initial.as_ptr(), 0o644) };
        assert_ne!(ino, 0, "create: {}", last_err());

        // write content
        let n = unsafe {
            fs_ext4_write_file(
                fs_h,
                p_initial.as_ptr(),
                payload.as_ptr() as *const c_void,
                payload.len() as u64,
            )
        };
        assert_eq!(n, payload.len() as i64, "write: {}", last_err());

        // rename into subdir
        let rc = unsafe { fs_ext4_rename(fs_h, p_initial.as_ptr(), p_renamed.as_ptr()) };
        assert_eq!(rc, 0, "rename: {}", last_err());

        // read after rename — same payload
        let mut buf = [0u8; 64];
        let n = unsafe {
            fs_ext4_read_file(
                fs_h,
                p_renamed.as_ptr(),
                buf.as_mut_ptr() as *mut c_void,
                0,
                buf.len() as u64,
            )
        };
        assert_eq!(n as usize, payload.len());
        assert_eq!(&buf[..payload.len()], payload);

        // truncate down to half
        let half = (payload.len() / 2) as u64;
        let rc = unsafe { fs_ext4_truncate(fs_h, p_renamed.as_ptr(), half) };
        assert_eq!(rc, 0, "truncate: {}", last_err());

        // read after truncate — prefix only
        let mut buf = [0u8; 64];
        let n = unsafe {
            fs_ext4_read_file(
                fs_h,
                p_renamed.as_ptr(),
                buf.as_mut_ptr() as *mut c_void,
                0,
                buf.len() as u64,
            )
        };
        assert_eq!(n as u64, half);
        assert_eq!(&buf[..half as usize], &payload[..half as usize]);

        // unlink
        let rc = unsafe { fs_ext4_unlink(fs_h, p_renamed.as_ptr()) };
        assert_eq!(rc, 0, "unlink: {}", last_err());

        unsafe { fs_ext4_umount(fs_h) };
    }

    // Remount ro — full csum chain must pass, files should be gone.
    {
        let fs_h = unsafe { fs_ext4_mount(img_c.as_ptr()) };
        assert!(!fs_h.is_null(), "remount after chain: {}", last_err());

        let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
        let rc_a = unsafe { fs_ext4_stat(fs_h, p_initial.as_ptr(), &mut attr) };
        assert_eq!(rc_a, -1, "/chain.txt should be gone");
        let rc_b = unsafe { fs_ext4_stat(fs_h, p_renamed.as_ptr(), &mut attr) };
        assert_eq!(rc_b, -1, "/subdir/chain_moved.txt should be gone");

        unsafe { fs_ext4_umount(fs_h) };
    }

    let _ = fs::remove_file(&img);
}

#[test]
fn create_unlink_create_same_name_reuses_cleanly() {
    let img = scratch("reuse");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path = CString::new("/toggle.txt").unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    for _ in 0..3 {
        let ino = unsafe { fs_ext4_create(fs_h, path.as_ptr(), 0o644) };
        assert_ne!(ino, 0, "create in loop: {}", last_err());
        let rc = unsafe { fs_ext4_unlink(fs_h, path.as_ptr()) };
        assert_eq!(rc, 0, "unlink in loop: {}", last_err());
    }

    // After 3 create+unlink cycles the dir should not have accumulated
    // stale entries or stale blocks.
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_ext4_stat(fs_h, path.as_ptr(), &mut attr) };
    assert_eq!(rc, -1, "path should be absent after final unlink");

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn many_creates_in_root_do_not_collide() {
    // Create 20 distinct files, verify each stats correctly and has
    // a unique inode number.
    let img = scratch("many");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let mut inodes = Vec::with_capacity(20);
    for i in 0..20 {
        let name = format!("/f{i:02}.txt");
        let c = CString::new(name.clone()).unwrap();
        let ino = unsafe { fs_ext4_create(fs_h, c.as_ptr(), 0o644) };
        assert_ne!(ino, 0, "create {name}: {}", last_err());
        inodes.push(ino);
    }

    // All inodes distinct.
    let mut sorted = inodes.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), inodes.len(), "inode collisions: {inodes:?}");

    // Stat each back.
    for (i, &expected_ino) in inodes.iter().enumerate() {
        let name = format!("/f{i:02}.txt");
        let c = CString::new(name).unwrap();
        let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
        let rc = unsafe { fs_ext4_stat(fs_h, c.as_ptr(), &mut attr) };
        assert_eq!(rc, 0);
        assert_eq!(attr.inode, expected_ino);
    }

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}
