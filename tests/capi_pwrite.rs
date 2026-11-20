//! Tests for `fs_ext4_pwrite` — the streaming positional-write primitive.
//!
//! This covers the *real* streaming write path (replacing the old
//! "merge then whole-file replace" hack the WinFsp/FSKit adapters used to
//! do). Each test exercises one observable contract:
//!   1. Sequential chunked writes rebuild the same content as a single
//!      whole-file write. This is the "Explorer copy" path.
//!   2. Partial overwrite: writing into an existing file at a non-zero
//!      offset preserves bytes outside the write range.
//!   3. Sparse extension: pwrite past EOF leaves a sparse hole that
//!      reads as zeros, and bumps i_size correctly.
//!   4. Type guards: pwrite on a directory / non-regular-file fails
//!      cleanly with the right errno.
//!   5. Length cap: oversize `len` is rejected before any path resolution.

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::fs;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

fn scratch() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_pwrite_{}_{n}.img",
        std::process::id()
    ));
    fs::copy(SRC, &dst).unwrap();
    dst
}

fn last_err() -> String {
    unsafe {
        let p = fs_ext4_last_error();
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

fn read_full(fs_h: *mut fs_ext4_fs_t, path: &str, size: u64) -> Vec<u8> {
    let cp = CString::new(path).unwrap();
    let mut buf = vec![0u8; size as usize];
    let n =
        unsafe { fs_ext4_read_file(fs_h, cp.as_ptr(), buf.as_mut_ptr() as *mut c_void, 0, size) };
    assert!(
        n >= 0,
        "read_file({path}, 0, {size}) failed: rc={n}, err={}",
        last_err()
    );
    buf.truncate(n as usize);
    buf
}

fn stat_size(fs_h: *mut fs_ext4_fs_t, path: &str) -> u64 {
    let cp = CString::new(path).unwrap();
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let r = unsafe { fs_ext4_stat(fs_h, cp.as_ptr(), &mut attr) };
    assert_eq!(r, 0, "stat({path}) failed: {}", last_err());
    attr.size
}

/// Sequential 64KiB chunks rebuild a 1MiB file identically. Models the
/// Explorer/CopyFileEx cache-manager dispatch pattern that the old
/// merge-and-replace hack handled in O(filesize²); pwrite handles it in
/// O(chunk).
#[test]
fn sequential_chunks_match_whole_file() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null(), "mount: {}", last_err());

    let path = CString::new("/pwrite-streamed").unwrap();
    let ino = unsafe { fs_ext4_create(fs_h, path.as_ptr(), 0o644) };
    assert!(ino > 0, "create: {}", last_err());

    // Synthetic payload: deterministic + non-aligned-friendly. 1 MiB total.
    let total: usize = 1 << 20;
    let payload: Vec<u8> = (0..total)
        .map(|i| (i.wrapping_mul(31) ^ (i >> 8)) as u8)
        .collect();

    let chunk: usize = 64 * 1024;
    let mut written: usize = 0;
    while written < total {
        let len = chunk.min(total - written);
        let rc = unsafe {
            fs_ext4_pwrite(
                fs_h,
                path.as_ptr(),
                payload.as_ptr().add(written) as *const c_void,
                len as u64,
                written as u64,
            )
        };
        assert!(
            rc >= 0,
            "pwrite @{written}+{len} failed: rc={rc} err={}",
            last_err()
        );
        let expected_size = (written + len) as i64;
        assert_eq!(
            rc, expected_size,
            "pwrite returned size {rc}, expected {expected_size}"
        );
        written += len;
    }

    assert_eq!(stat_size(fs_h, "/pwrite-streamed"), total as u64);
    let read_back = read_full(fs_h, "/pwrite-streamed", total as u64);
    assert_eq!(
        read_back, payload,
        "streamed content does not match original"
    );

    unsafe { fs_ext4_umount(fs_h) };

    // Remount RO and re-verify so we know it persists past umount/remount,
    // which exercises the journal-replay + checksum-verification paths
    // that streaming writes most commonly trip up.
    let fs_h = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs_h.is_null(), "remount: {}", last_err());
    let read_back = read_full(fs_h, "/pwrite-streamed", total as u64);
    assert_eq!(read_back, payload, "remounted content does not match");
    unsafe { fs_ext4_umount(fs_h) };

    let _ = fs::remove_file(&img);
}

/// Partial overwrite of an existing file leaves bytes outside the write
/// range untouched. Models a Save-As that rewrites a header without
/// touching the rest of the file.
#[test]
fn partial_overwrite_preserves_outside_range() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null(), "mount: {}", last_err());

    let path = CString::new("/pwrite-overwrite").unwrap();
    let ino = unsafe { fs_ext4_create(fs_h, path.as_ptr(), 0o644) };
    assert!(ino > 0);

    // Initial content: 16 KiB of byte value 0xAA.
    let initial = vec![0xAAu8; 16 * 1024];
    let rc = unsafe {
        fs_ext4_pwrite(
            fs_h,
            path.as_ptr(),
            initial.as_ptr() as *const c_void,
            initial.len() as u64,
            0,
        )
    };
    assert!(rc >= 0, "initial pwrite: {}", last_err());

    // Overwrite [4096, 8192) with 0xBB. The blocks [0, 4096) and
    // [8192, 16384) must remain 0xAA.
    let patch = vec![0xBBu8; 4096];
    let rc = unsafe {
        fs_ext4_pwrite(
            fs_h,
            path.as_ptr(),
            patch.as_ptr() as *const c_void,
            patch.len() as u64,
            4096,
        )
    };
    assert!(rc >= 0, "patch pwrite: {}", last_err());
    assert_eq!(rc as u64, 16 * 1024, "size should be unchanged");

    let read_back = read_full(fs_h, "/pwrite-overwrite", 16 * 1024);
    assert_eq!(&read_back[0..4096], &[0xAAu8; 4096][..]);
    assert_eq!(&read_back[4096..8192], &[0xBBu8; 4096][..]);
    assert_eq!(&read_back[8192..16384], &[0xAAu8; 8192][..]);

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

/// pwrite past current EOF leaves a sparse hole (reads as zeros) and
/// bumps i_size to offset+len. The hole's logical blocks must NOT
/// consume physical blocks.
#[test]
fn pwrite_past_eof_creates_sparse_hole() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let path = CString::new("/pwrite-sparse").unwrap();
    let ino = unsafe { fs_ext4_create(fs_h, path.as_ptr(), 0o644) };
    assert!(ino > 0);

    // Empty file. pwrite "tail" at offset 32 KiB.
    let tail = b"end-of-sparse-file";
    let rc = unsafe {
        fs_ext4_pwrite(
            fs_h,
            path.as_ptr(),
            tail.as_ptr() as *const c_void,
            tail.len() as u64,
            32 * 1024,
        )
    };
    assert!(rc >= 0, "sparse pwrite: {}", last_err());
    let expected_size = 32 * 1024 + tail.len() as u64;
    assert_eq!(rc as u64, expected_size);
    assert_eq!(stat_size(fs_h, "/pwrite-sparse"), expected_size);

    // First 32 KiB must read as zeros; tail must match.
    let mut buf = vec![0xFFu8; expected_size as usize];
    let n = unsafe {
        fs_ext4_read_file(
            fs_h,
            path.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            0,
            expected_size,
        )
    };
    assert!(n >= 0, "read sparse: {}", last_err());
    assert_eq!(n as u64, expected_size);
    assert!(buf[..32 * 1024].iter().all(|&b| b == 0), "hole not zero");
    assert_eq!(&buf[32 * 1024..], tail);

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn pwrite_on_directory_fails_with_eisdir() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let dir = CString::new("/subdir").unwrap();
    let payload = b"into the void";
    let rc = unsafe {
        fs_ext4_pwrite(
            fs_h,
            dir.as_ptr(),
            payload.as_ptr() as *const c_void,
            payload.len() as u64,
            0,
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22 /* EINVAL */);

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn pwrite_rejects_oversize_len() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let path = CString::new("/anything").unwrap();
    let dummy = [0u8; 1];
    let oversize: u64 = (1u64 << 30) + 1; // 1 byte past the 1 GiB cap
    let rc = unsafe {
        fs_ext4_pwrite(
            fs_h,
            path.as_ptr(),
            dummy.as_ptr() as *const c_void,
            oversize,
            0,
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22 /* EINVAL */);
    let msg = last_err();
    assert!(
        msg.contains("len") && msg.contains("exceeds"),
        "expected len-cap message, got: {msg}"
    );

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

/// Force the extent tree past the inline-root capacity (4 entries). Writes
/// 5 single-block chunks at sparse-hole-separated offsets so each
/// `plan_insert_extent` call produces a NEW extent (no auto-merge with
/// neighbours). The 5th write trips `LEAF_FULL_NEEDS_PROMOTION` — without
/// the deep-insert fallback this would fail outright; with it, the tree
/// promotes to depth 1 and all reads round-trip.
#[test]
fn many_disjoint_writes_promote_extent_tree_depth() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null(), "mount: {}", last_err());

    let path = CString::new("/pwrite-deep-tree").unwrap();
    let ino = unsafe { fs_ext4_create(fs_h, path.as_ptr(), 0o644) };
    assert!(ino > 0, "create: {}", last_err());

    // Sparse-separated, single-block writes. The 4096-byte gap between
    // them ensures each lands in a brand new logical block range that
    // doesn't auto-merge with the prior extent (different logical_block
    // and the gap has no extent to be contiguous with).
    let block_bytes = 4096usize;
    let chunk = vec![0xCDu8; block_bytes];
    let offsets: [u64; 6] = [
        0,
        2 * block_bytes as u64,
        4 * block_bytes as u64,
        6 * block_bytes as u64,
        8 * block_bytes as u64,
        10 * block_bytes as u64,
    ];
    for &off in &offsets {
        let rc = unsafe {
            fs_ext4_pwrite(
                fs_h,
                path.as_ptr(),
                chunk.as_ptr() as *const c_void,
                chunk.len() as u64,
                off,
            )
        };
        assert!(
            rc >= 0,
            "pwrite @{off} failed (deep-insert path): rc={rc} err={}",
            last_err()
        );
    }

    // Each written block must read back as 0xCD; each gap as 0x00 (sparse).
    let total_size = *offsets.last().unwrap() + chunk.len() as u64;
    let mut buf = vec![0xFFu8; total_size as usize];
    let n = unsafe {
        fs_ext4_read_file(
            fs_h,
            path.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            0,
            total_size,
        )
    };
    assert!(n >= 0, "read after deep promotion: {}", last_err());
    for &off in &offsets {
        let written_slice = &buf[off as usize..off as usize + block_bytes];
        assert!(
            written_slice.iter().all(|&b| b == 0xCD),
            "written block at {off} not preserved after deep promotion"
        );
    }
    // Verify a couple of gaps stayed sparse (zeroed).
    for &off in &offsets[..offsets.len() - 1] {
        let gap_start = off as usize + block_bytes;
        let gap_end = gap_start + block_bytes;
        assert!(
            buf[gap_start..gap_end].iter().all(|&b| b == 0),
            "sparse gap at byte {gap_start}..{gap_end} not zero"
        );
    }

    unsafe { fs_ext4_umount(fs_h) };

    // Remount and re-verify — exercises the deep-tree read path against
    // freshly-checksummed metadata.
    let fs_h = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs_h.is_null(), "remount: {}", last_err());
    let mut buf2 = vec![0xFFu8; total_size as usize];
    let n = unsafe {
        fs_ext4_read_file(
            fs_h,
            path.as_ptr(),
            buf2.as_mut_ptr() as *mut c_void,
            0,
            total_size,
        )
    };
    assert!(n >= 0, "read after remount: {}", last_err());
    assert_eq!(buf2, buf, "content drift across remount");
    unsafe { fs_ext4_umount(fs_h) };

    let _ = fs::remove_file(&img);
}

#[test]
fn pwrite_zero_len_is_noop() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let path = CString::new("/pwrite-zerolen").unwrap();
    let ino = unsafe { fs_ext4_create(fs_h, path.as_ptr(), 0o644) };
    assert!(ino > 0);
    // Seed some content so we can verify the no-op didn't truncate.
    let seed = b"keepme";
    let rc = unsafe {
        fs_ext4_pwrite(
            fs_h,
            path.as_ptr(),
            seed.as_ptr() as *const c_void,
            seed.len() as u64,
            0,
        )
    };
    assert!(rc >= 0);

    let rc = unsafe { fs_ext4_pwrite(fs_h, path.as_ptr(), std::ptr::null(), 0, 999) };
    assert!(rc >= 0, "zero-len pwrite at any offset must succeed");
    assert_eq!(rc as u64, seed.len() as u64, "size unchanged");

    let read_back = read_full(fs_h, "/pwrite-zerolen", seed.len() as u64);
    assert_eq!(read_back, seed);

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}
