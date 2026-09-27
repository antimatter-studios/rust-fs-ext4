//! Integration tests for long-symlink (slow-symlink) creation. The driver
//! routes symlinks two ways:
//!   - target.len() <  60: fast path — bytes stored inline in the inode's
//!     60-byte i_block area, no data block, no extent tree.
//!   - target.len() >= 60: slow path — one fs block allocated, target bytes
//!     written there (zero-padded to block_size), single extent inserted
//!     into the inode's extent root.
//!
//! Linux's `ext4_symlink` uses the slow path when target length is >=
//! sizeof(i_block) (i.e. >= 60). We match that boundary on both the write
//! and read sides so a 60-byte target is consistently treated as slow.

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
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
        "fs_ext4_capi_long_symlink_{tag}_{}_{n}.img",
        std::process::id()
    ));
    let bytes = fs::read(SRC).expect("read src");
    let mut out = fs::File::create(&dst).expect("create");
    out.write_all(&bytes).expect("write");
    out.flush().expect("flush");
    dst
}

fn last_err() -> String {
    unsafe {
        CStr::from_ptr(fs_ext4_last_error())
            .to_string_lossy()
            .into_owned()
    }
}

fn stat_attr(fs_handle: *mut fs_ext4_fs_t, path: &str) -> fs_ext4_attr_t {
    let p = CString::new(path).unwrap();
    let mut attr = MaybeUninit::<fs_ext4_attr_t>::uninit();
    let rc = unsafe { fs_ext4_stat(fs_handle, p.as_ptr(), attr.as_mut_ptr()) };
    assert_eq!(rc, 0, "stat {path} failed: {}", last_err());
    unsafe { attr.assume_init() }
}

fn free_blocks(fs_handle: *mut fs_ext4_fs_t) -> u64 {
    let mut info = MaybeUninit::<fs_ext4_volume_info_t>::uninit();
    let rc = unsafe { fs_ext4_get_volume_info(fs_handle, info.as_mut_ptr()) };
    assert_eq!(rc, 0, "get_volume_info failed: {}", last_err());
    unsafe { info.assume_init() }.free_blocks
}

fn make_symlink(fs_handle: *mut fs_ext4_fs_t, target: &str, linkpath: &str) -> u32 {
    let target_c = CString::new(target).unwrap();
    let link_c = CString::new(linkpath).unwrap();
    unsafe { fs_ext4_symlink(fs_handle, target_c.as_ptr(), link_c.as_ptr()) }
}

fn readlink_to_bytes(fs_handle: *mut fs_ext4_fs_t, linkpath: &str, cap: usize) -> Vec<u8> {
    let link_c = CString::new(linkpath).unwrap();
    let mut buf = vec![0u8; cap];
    let rc = unsafe {
        fs_ext4_readlink(
            fs_handle,
            link_c.as_ptr(),
            buf.as_mut_ptr() as *mut _,
            buf.len(),
        )
    };
    assert!(rc >= 0, "readlink {linkpath} failed: {}", last_err());
    let nul = buf.iter().position(|&b| b == 0).expect("NUL terminator");
    assert_eq!(
        rc as usize, nul,
        "readlink {linkpath}: return is the target length"
    );
    buf.truncate(nul);
    buf
}

/// Determine fast-vs-slow by comparing on-disk free_blocks before/after.
/// `fs.sb.free_blocks_count` in the C ABI volume info is the cached
/// parse-time value and isn't updated by writes — we have to umount and
/// remount to see the post-write count.
fn free_blocks_via_remount(img_path: &CString) -> u64 {
    let h = unsafe { fs_ext4_mount(img_path.as_ptr()) };
    assert!(!h.is_null(), "remount for free_blocks check failed");
    let n = free_blocks(h);
    unsafe { fs_ext4_umount(h) };
    n
}

#[test]
fn fast_path_30_byte_target_unchanged() {
    let target = "x".repeat(30);
    let img = scratch("fast30");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let before = free_blocks_via_remount(&img_c);

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let ino = make_symlink(fs_h, &target, "/short");
    assert!(ino > 0, "symlink failed: {}", last_err());

    let attr = stat_attr(fs_h, "/short");
    assert_eq!(attr.size, 30);

    let got = readlink_to_bytes(fs_h, "/short", 256);
    assert_eq!(got, target.as_bytes());

    unsafe { fs_ext4_umount(fs_h) };

    let after = free_blocks_via_remount(&img_c);
    assert_eq!(
        before, after,
        "fast symlink should not consume any data blocks"
    );

    let _ = fs::remove_file(&img);
}

#[test]
fn fast_path_59_byte_target_uses_inline_storage() {
    // 59 bytes — last fast-path size (boundary is target.len() < 60).
    let target = "y".repeat(59);
    let img = scratch("fast59");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let before = free_blocks_via_remount(&img_c);

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let ino = make_symlink(fs_h, &target, "/at59");
    assert!(ino > 0, "symlink failed: {}", last_err());

    let attr = stat_attr(fs_h, "/at59");
    assert_eq!(attr.size, 59);

    let got = readlink_to_bytes(fs_h, "/at59", 128);
    assert_eq!(got, target.as_bytes());

    unsafe { fs_ext4_umount(fs_h) };

    let after = free_blocks_via_remount(&img_c);
    assert_eq!(
        before, after,
        "59-byte target must stay inline; no data block should be allocated"
    );
    let _ = fs::remove_file(&img);
}

#[test]
fn boundary_60_byte_target_takes_slow_path() {
    // ext4 spec / Linux ext4_symlink: switch to slow when len >= sizeof(i_block).
    // i_block is 60 bytes, so target.len() == 60 must use slow (one block
    // allocated, extent inserted). The read path checks `inode.size < 60`
    // for the fast branch; mismatched boundaries would corrupt readlink.
    let target = "z".repeat(60);
    let img = scratch("at60");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let before = free_blocks_via_remount(&img_c);

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let ino = make_symlink(fs_h, &target, "/at60");
    assert!(ino > 0, "symlink failed: {}", last_err());

    let attr = stat_attr(fs_h, "/at60");
    assert_eq!(attr.size, 60);

    let got = readlink_to_bytes(fs_h, "/at60", 128);
    assert_eq!(got, target.as_bytes());

    unsafe { fs_ext4_umount(fs_h) };

    let after = free_blocks_via_remount(&img_c);
    // Slow path: exactly one fs block consumed.
    assert_eq!(
        before - after,
        1,
        "60-byte target must take slow path; expected 1 fs block consumed"
    );

    // Re-verify the readlink survives a clean remount (catches checksum bugs
    // on the slow boundary).
    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount failed: {}", last_err());
    let got2 = readlink_to_bytes(fs2, "/at60", 128);
    assert_eq!(got2, target.as_bytes());
    unsafe { fs_ext4_umount(fs2) };

    let _ = fs::remove_file(&img);
}

#[test]
fn slow_path_200_byte_target_roundtrips() {
    let target: String = (0..200).map(|i| (b'a' + (i as u8 % 26)) as char).collect();
    assert_eq!(target.len(), 200);

    let img = scratch("slow200");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let before = free_blocks_via_remount(&img_c);

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let ino = make_symlink(fs_h, &target, "/two_hundred");
    assert!(ino > 0, "symlink failed: {}", last_err());

    let attr = stat_attr(fs_h, "/two_hundred");
    assert_eq!(attr.size, 200);

    let got = readlink_to_bytes(fs_h, "/two_hundred", 512);
    assert_eq!(got, target.as_bytes());

    unsafe { fs_ext4_umount(fs_h) };

    let after = free_blocks_via_remount(&img_c);
    assert_eq!(before - after, 1, "expected one fs block consumed");
    let _ = fs::remove_file(&img);
}

#[test]
fn slow_path_1000_byte_target_roundtrips() {
    let target: String = (0..1000).map(|i| (b'A' + (i as u8 % 26)) as char).collect();
    assert_eq!(target.len(), 1000);

    let img = scratch("slow1000");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let before = free_blocks_via_remount(&img_c);

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let ino = make_symlink(fs_h, &target, "/big");
    assert!(ino > 0, "symlink failed: {}", last_err());

    let attr = stat_attr(fs_h, "/big");
    assert_eq!(attr.size, 1000);

    let got = readlink_to_bytes(fs_h, "/big", 4096);
    assert_eq!(got, target.as_bytes());

    unsafe { fs_ext4_umount(fs_h) };

    let after = free_blocks_via_remount(&img_c);
    // Still one block — 1000 bytes fits comfortably in 4 KiB.
    assert_eq!(before - after, 1, "expected one fs block consumed");

    let _ = fs::remove_file(&img);
}

#[test]
fn target_5000_bytes_returns_enametoolong() {
    // 5000 > FFI_PATH_MAX (4096). The FFI surface rejects this with
    // ENAMETOOLONG (errno 63) before reaching apply_symlink.
    let target = "q".repeat(5000);
    let img = scratch("over_path_max");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let ino = make_symlink(fs_h, &target, "/nope");
    assert_eq!(ino, 0);
    assert_eq!(
        fs_ext4_last_errno(),
        63,
        "expected ENAMETOOLONG, got {} ({})",
        fs_ext4_last_errno(),
        last_err()
    );

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn slow_symlink_survives_csum_validated_remount() {
    // ext4-basic.img has metadata_csum on; this test verifies the
    // slow-symlink write path patches inode + (where applicable) extent
    // checksums correctly. After umount and a default (csum-verifying)
    // remount, readlink must still return the original target bytes.
    let target = "/this/is/a/relative-ish/path/with/many/components/".to_string()
        + &"deep/".repeat(20)
        + "leaf.bin";
    assert!(target.len() >= 60);
    assert!(target.len() <= 4096);

    let img = scratch("remount_csum");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let ino = make_symlink(fs_h, &target, "/persist_link");
    assert!(ino > 0, "symlink failed: {}", last_err());
    let pre = readlink_to_bytes(fs_h, "/persist_link", 4096);
    assert_eq!(pre, target.as_bytes());
    unsafe { fs_ext4_umount(fs_h) };

    // Default remount = csum-verifying RO mount. If inode/extent csum was
    // miscomputed during create, mount or readlink will fail.
    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(
        !fs2.is_null(),
        "RO csum-verified remount failed: {}",
        last_err()
    );

    let attr = stat_attr(fs2, "/persist_link");
    assert!(matches!(attr.file_type, fs_ext4_file_type_t::Symlink));
    assert_eq!(attr.size, target.len() as u64);

    let got = readlink_to_bytes(fs2, "/persist_link", 4096);
    assert_eq!(got, target.as_bytes());

    unsafe { fs_ext4_umount(fs2) };
    let _ = fs::remove_file(&img);
}

#[test]
fn slow_symlink_1000_bytes_survives_remount() {
    let target: String = (0..1000).map(|i| (b'a' + (i as u8 % 26)) as char).collect();

    let img = scratch("remount_1k");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let ino = make_symlink(fs_h, &target, "/big_persist");
    assert!(ino > 0, "symlink failed: {}", last_err());
    unsafe { fs_ext4_umount(fs_h) };

    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount failed: {}", last_err());
    let got = readlink_to_bytes(fs2, "/big_persist", 4096);
    assert_eq!(got, target.as_bytes());
    unsafe { fs_ext4_umount(fs2) };
    let _ = fs::remove_file(&img);
}
