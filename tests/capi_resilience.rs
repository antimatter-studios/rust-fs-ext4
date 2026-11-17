//! Corruption / resilience tests for the C ABI.
//!
//! A filesystem driver running inside a FSKit extension receives whatever
//! bytes come off the block device. If the disk is corrupt (bad sectors,
//! truncated image, flipped bits), the driver MUST return a clean error —
//! never panic, never abort, never undefined-behaviour across the FFI.
//!
//! Strategy: take a known-good image, copy it into tmp, deterministically
//! flip bytes in specific critical regions, then invoke the full C ABI on
//! it. All paths must either succeed (with sane results) or return the
//! sentinel error with `fs_ext4_last_errno() != 0` and a non-empty
//! `fs_ext4_last_error()`.

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::Arc;

#[track_caller]
fn good_image() -> String {
    fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), "ext4-basic.img")
}

fn last_err() -> String {
    unsafe {
        let p = fs_ext4_last_error();
        if p.is_null() {
            return String::new();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// Make a tmp copy of ext4-basic.img, apply `mutate` to the bytes, write
/// it back out, and return the tmp path (caller is responsible for drop).
fn corrupted_copy(label: &str, mutate: impl FnOnce(&mut File, u64)) -> PathBuf {
    let mut p = fs_ext4_test_support::temp_dir().to_path_buf();
    p.push(format!(
        "ext4rs-resilience-{label}-{}.img",
        std::process::id()
    ));
    fs_ext4_test_support::copy_file_sparse(good_image(), &p).expect("copy source image");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&p)
        .expect("open corrupted image");
    let length = file.metadata().expect("image metadata").len();
    mutate(&mut file, length);
    p
}

/// Invoke the full C-ABI smoke sequence on `path` and assert that nothing
/// panics. Regardless of whether the mount succeeds, all operations must
/// return cleanly (no UB, no abort).
fn hammer_all_entry_points(path: &str) {
    let c_path = CString::new(path).unwrap();
    let fs = unsafe { fs_ext4_mount(c_path.as_ptr()) };

    if fs.is_null() {
        // Mount rejected — verify error plumbing worked.
        let errno = fs_ext4_last_errno();
        assert_ne!(errno, 0, "mount failed but errno is 0 for {path}");
        assert!(
            !last_err().is_empty(),
            "mount failed but last_error is empty"
        );
        return;
    }

    // Mount succeeded — try the full API surface. None of these should panic.
    let root = CString::new("/").unwrap();
    let any = CString::new("/test.txt").unwrap();
    let nope = CString::new("/does-not-exist").unwrap();

    // Volume info.
    let mut info: fs_ext4_volume_info_t = unsafe { std::mem::zeroed() };
    let _ = unsafe { fs_ext4_get_volume_info(fs, &mut info) };

    // Stat variants.
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let _ = unsafe { fs_ext4_stat(fs, root.as_ptr(), &mut attr) };
    let _ = unsafe { fs_ext4_stat(fs, any.as_ptr(), &mut attr) };
    let _ = unsafe { fs_ext4_stat(fs, nope.as_ptr(), &mut attr) };

    // Directory walk.
    let iter = unsafe { fs_ext4_dir_open(fs, root.as_ptr()) };
    if !iter.is_null() {
        loop {
            let e = unsafe { fs_ext4_dir_next(iter) };
            if e.is_null() {
                break;
            }
        }
        unsafe { fs_ext4_dir_close(iter) };
    }

    // File read.
    let mut buf = [0u8; 256];
    let _ = unsafe {
        fs_ext4_read_file(
            fs,
            any.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            0,
            buf.len() as u64,
        )
    };

    // xattrs.
    let _ = unsafe { fs_ext4_listxattr(fs, any.as_ptr(), std::ptr::null_mut(), 0) };
    let nm = CString::new("user.whatever").unwrap();
    let _ = unsafe { fs_ext4_getxattr(fs, any.as_ptr(), nm.as_ptr(), std::ptr::null_mut(), 0) };

    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn known_good_image_baseline() {
    // Sanity check — the unmodified image must mount cleanly.
    let c = CString::new(good_image()).unwrap();
    let fs = unsafe { fs_ext4_mount(c.as_ptr()) };
    assert!(!fs.is_null(), "baseline mount failed: {}", last_err());
    assert_eq!(fs_ext4_last_errno(), 0);
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn trashed_superblock_magic_rejected_cleanly() {
    // Superblock starts at byte offset 1024; magic is 2 bytes at offset
    // 56 within the superblock (s_magic = 0xEF53). Flip it to 0xDEAD.
    let p = corrupted_copy("bad-magic", |file, _| {
        file.seek(SeekFrom::Start(1024 + 56)).unwrap();
        file.write_all(&[0xAD, 0xDE]).unwrap();
    });
    hammer_all_entry_points(p.to_str().unwrap());
    let _ = fs::remove_file(&p);
}

#[test]
fn truncated_image_rejected_cleanly() {
    // Truncate to something smaller than the superblock offset (1024).
    let p = corrupted_copy("truncated", |file, _| {
        file.set_len(512).unwrap();
    });
    hammer_all_entry_points(p.to_str().unwrap());
    let _ = fs::remove_file(&p);
}

#[test]
fn zeroed_superblock_rejected_cleanly() {
    let p = corrupted_copy("zero-sb", |file, _| {
        file.seek(SeekFrom::Start(1024)).unwrap();
        file.write_all(&[0; 1024]).unwrap();
    });
    hammer_all_entry_points(p.to_str().unwrap());
    let _ = fs::remove_file(&p);
}

#[test]
fn trashed_bgd_area_handled_cleanly() {
    // Block group descriptor table follows the superblock, starting at
    // block 1 (offset 4096 for 4KB blocks). Poison 512 bytes of it.
    let p = corrupted_copy("bad-bgd", |file, _| {
        file.seek(SeekFrom::Start(4096)).unwrap();
        file.write_all(&[0xAA; 512]).unwrap();
    });
    hammer_all_entry_points(p.to_str().unwrap());
    let _ = fs::remove_file(&p);
}

#[test]
fn trashed_inode_table_handled_cleanly() {
    // The root inode (inode #2) lives at a computed offset in the inode
    // table. Rather than compute it exactly, trash a wider range likely
    // to hit it — around block ~36 for this mkfs layout.
    let p = corrupted_copy("bad-inode", |file, length| {
        let start = 36 * 4096;
        if start + 4096 < length as usize {
            file.seek(SeekFrom::Start(start as u64)).unwrap();
            file.write_all(&[0xEE; 4096]).unwrap();
        }
    });
    hammer_all_entry_points(p.to_str().unwrap());
    let _ = fs::remove_file(&p);
}

#[test]
fn random_bit_flips_in_data_area_handled_cleanly() {
    // Scatter flips deep in the image (past metadata). Using a fixed seed
    // so the test is deterministic.
    let p = corrupted_copy("scattered", |file, length| {
        let mut seed: u32 = 0x9E37_79B9;
        for _ in 0..256 {
            // xorshift32
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let idx = u64::from(seed) % length;
            if idx > 8192 {
                file.seek(SeekFrom::Start(idx)).unwrap();
                let mut byte = [0];
                file.read_exact(&mut byte).unwrap();
                file.seek(SeekFrom::Start(idx)).unwrap();
                file.write_all(&[byte[0] ^ 0xFF]).unwrap();
            }
        }
    });
    hammer_all_entry_points(p.to_str().unwrap());
    let _ = fs::remove_file(&p);
}

#[test]
fn fully_zeroed_image_rejected_cleanly() {
    let p = corrupted_copy("all-zero", |file, length| {
        let zeros = [0; 1024 * 1024];
        let mut offset = 0;
        while offset < length {
            let count = (length - offset).min(zeros.len() as u64) as usize;
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(&zeros[..count]).unwrap();
            offset += count as u64;
        }
    });
    hammer_all_entry_points(p.to_str().unwrap());
    let _ = fs::remove_file(&p);
}

// ---------------------------------------------------------------------------
// Callback-based mount path (what FSKit actually uses in production)
// ---------------------------------------------------------------------------

/// A read callback that serves bytes from the file device pointed to by `ctx`.
/// Matches the `fs_ext4_read_fn` signature.
extern "C" fn read_from_file(
    ctx: *mut c_void,
    buf: *mut c_void,
    offset: u64,
    length: u64,
) -> std::os::raw::c_int {
    if ctx.is_null() || buf.is_null() {
        return 1;
    }
    let device = unsafe { &*(ctx as *const FileDevice) };
    let Ok(length) = usize::try_from(length) else {
        return 2;
    };
    let bytes = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, length) };
    device.read_at(offset, bytes).map_or(2, |_| 0)
}

fn mount_callback(device: &Arc<FileDevice>) -> *mut fs_ext4_fs_t {
    let cfg = fs_ext4_blockdev_cfg_t {
        read: Some(read_from_file),
        context: Arc::as_ptr(device) as *mut c_void,
        size_bytes: device.size_bytes(),
        block_size: 512,
        write: None,
        flush: None,
    };
    unsafe { fs_ext4_mount_with_callbacks(&cfg) }
}

#[test]
fn callback_mount_succeeds_on_good_image() {
    let path = good_image();
    let device = Arc::new(FileDevice::open(&path).unwrap());
    let fs = mount_callback(&device);
    assert!(!fs.is_null(), "callback mount failed: {}", last_err());
    assert_eq!(fs_ext4_last_errno(), 0);
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn callback_mount_rejects_corrupted_bytes_cleanly() {
    let path = corrupted_copy("callback-bad-magic", |file, _| {
        file.seek(SeekFrom::Start(1024 + 56)).unwrap();
        file.write_all(&[0, 0]).unwrap();
    });
    let device = Arc::new(FileDevice::open(path.to_str().unwrap()).unwrap());
    let fs = mount_callback(&device);
    assert!(fs.is_null(), "corrupted callback mount must fail");
    assert_ne!(fs_ext4_last_errno(), 0);
    assert!(!last_err().is_empty());
}

#[test]
fn callback_mount_null_cfg_returns_einval() {
    let fs = unsafe { fs_ext4_mount_with_callbacks(std::ptr::null()) };
    assert!(fs.is_null());
    assert_eq!(fs_ext4_last_errno(), 22); // EINVAL
}

#[test]
fn callback_mount_with_failing_read_fn_rejected_cleanly() {
    // A callback that always errors must cause mount to fail cleanly.
    extern "C" fn always_fail(
        _ctx: *mut c_void,
        _buf: *mut c_void,
        _offset: u64,
        _length: u64,
    ) -> std::os::raw::c_int {
        5 // pretend EIO
    }
    let cfg = fs_ext4_blockdev_cfg_t {
        read: Some(always_fail),
        context: std::ptr::null_mut(),
        size_bytes: 16 * 1024 * 1024,
        block_size: 512,
        write: None,
        flush: None,
    };
    let fs_ptr = unsafe { fs_ext4_mount_with_callbacks(&cfg) };
    assert!(fs_ptr.is_null(), "mount must fail when callback errors");
    assert_ne!(fs_ext4_last_errno(), 0);
    assert!(!last_err().is_empty());
}

#[test]
fn callback_mount_null_read_fn_returns_einval() {
    let cfg = fs_ext4_blockdev_cfg_t {
        read: None,
        context: std::ptr::null_mut(),
        size_bytes: 0,
        block_size: 512,
        write: None,
        flush: None,
    };
    let fs = unsafe { fs_ext4_mount_with_callbacks(&cfg) };
    assert!(fs.is_null());
    assert_eq!(fs_ext4_last_errno(), 22); // EINVAL
}

// ---------------------------------------------------------------------------
// Null-pointer safety on the void-returning destructors
// ---------------------------------------------------------------------------

#[test]
fn umount_null_pointer_is_a_no_op() {
    // Swift could double-free if a mount retry reuses a stale pointer.
    // Passing null must not panic or crash — just return cleanly.
    unsafe { fs_ext4_umount(std::ptr::null_mut()) };
}

#[test]
fn dir_close_null_pointer_is_a_no_op() {
    unsafe { fs_ext4_dir_close(std::ptr::null_mut()) };
}

#[test]
fn dir_next_null_pointer_returns_null() {
    let p = unsafe { fs_ext4_dir_next(std::ptr::null_mut()) };
    assert!(p.is_null());
}

/// Real-world case: a user points the FSKit extension at a disk that is
/// not ext4. The mount must fail, and fail because the bytes are not ext4.
///
/// This named `test-disks/ntfs-basic.img`, which no builder has ever
/// produced, so it exercised the missing-file path and passed on a mount
/// that would have accepted anything (#87). The image is made here: an
/// NTFS boot sector at the front of 16 MiB, which is all a probe of the
/// volume start sees. It has to exist, and the refusal has to be the
/// superblock's, not the open's.
#[test]
fn ntfs_image_mounted_as_ext4_rejected_cleanly() {
    let path = fs_ext4_test_support::temp_path!("fs_ext4_ntfs_{}.img", std::process::id());
    let mut image = vec![0u8; 16 * 1024 * 1024];
    image[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]); // jump
    image[3..11].copy_from_slice(b"NTFS    "); // OEM ID
    image[11..13].copy_from_slice(&512u16.to_le_bytes()); // bytes per sector
    image[13] = 8; // sectors per cluster
    image[21] = 0xF8; // media descriptor
    image[40..48].copy_from_slice(&(16u64 * 1024 * 1024 / 512 - 1).to_le_bytes()); // total sectors
    image[48..56].copy_from_slice(&4u64.to_le_bytes()); // $MFT cluster
    image[510] = 0x55;
    image[511] = 0xAA;
    std::fs::write(&path, &image).expect("write the NTFS image");
    assert!(
        std::path::Path::new(&path).exists(),
        "fixture: the image exists"
    );

    let c = CString::new(path.as_str()).unwrap();
    let fs = unsafe { fs_ext4_mount(c.as_ptr()) };
    let err = last_err();
    let errno = fs_ext4_last_errno();
    let _ = std::fs::remove_file(&path);
    assert!(
        fs.is_null(),
        "mount of an NTFS image must NOT succeed as ext4"
    );
    assert_ne!(errno, 0, "rejected mount must have non-zero errno");
    assert!(
        err.contains("bad magic"),
        "the refusal must be that this is not ext4, not a failure to open: {err:?}"
    );
}
