//! Integration tests for `fs_ext4_utimens`.
//!
//! Covers:
//! - atime + mtime round-trip via `fs_ext4_stat`.
//! - `FS_EXT4_TIME_OMIT` on either _sec leaves the original alone.
//! - ctime bumps on every call (POSIX requirement).
//! - Missing-path / null-arg errnos.
//! - RO (read-only) mount refuses with a non-zero errno.
//! - Survives unmount → csum-validated remount.

use fs_ext4::capi::*;
use fs_ext4::fs::TIME_OMIT;
use std::ffi::CString;
use std::fs;
use std::mem::MaybeUninit;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_utimens_{tag}_{}_{n}.img",
        std::process::id()
    ));
    fs::copy(SRC, &dst).expect("copy src");
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
fn utimens_sets_both_and_bumps_ctime() {
    let img = scratch("basic");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());

    // 2000-01-01 and 2000-01-02 — distinctive values far from the
    // build timestamp of the test image.
    let a = 946_684_800i64;
    let m = 946_771_200i64;
    let rc = unsafe { fs_ext4_utimens(fs_handle, path_c.as_ptr(), a, 0, m, 0) };
    assert_eq!(rc, 0);
    assert_eq!(fs_ext4_last_errno(), 0);

    let after = stat_attr(fs_handle, "/test.txt");
    assert_eq!(after.atime, a);
    assert_eq!(after.mtime, m);
    // ctime must be recent (now), not one of the values above.
    assert!(after.ctime > a);
    assert!(after.ctime > m);

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn utimens_atime_sentinel_leaves_atime_alone() {
    let img = scratch("atime_sentinel");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());

    let before = stat_attr(fs_handle, "/test.txt");
    let fresh_m = 1_700_000_000i64;

    let rc = unsafe { fs_ext4_utimens(fs_handle, path_c.as_ptr(), TIME_OMIT, 0, fresh_m, 0) };
    assert_eq!(rc, 0);

    let after = stat_attr(fs_handle, "/test.txt");
    assert_eq!(after.atime, before.atime, "atime preserved by sentinel");
    assert_eq!(after.mtime, fresh_m, "mtime applied");

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn utimens_mtime_sentinel_leaves_mtime_alone() {
    let img = scratch("mtime_sentinel");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());

    let before = stat_attr(fs_handle, "/test.txt");
    let fresh_a = 1_700_000_000i64;

    let rc = unsafe { fs_ext4_utimens(fs_handle, path_c.as_ptr(), fresh_a, 0, TIME_OMIT, 0) };
    assert_eq!(rc, 0);

    let after = stat_attr(fs_handle, "/test.txt");
    assert_eq!(after.atime, fresh_a, "atime applied");
    assert_eq!(after.mtime, before.mtime, "mtime preserved by sentinel");

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn utimens_missing_path_sets_enoent() {
    let img = scratch("enoent");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let bad = CString::new("/nope_utimens_xyz.qqq").unwrap();
    let rc = unsafe { fs_ext4_utimens(fs_handle, bad.as_ptr(), 1, 0, 1, 0) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 2);
    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn utimens_null_args_set_einval() {
    let img = scratch("null");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let rc = unsafe { fs_ext4_utimens(fs_handle, std::ptr::null(), 1, 0, 1, 0) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22);
    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn utimens_survives_remount_with_csum() {
    let img = scratch("csum");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let a = 1_500_000_000i64;
    let m = 1_500_000_100i64;
    let rc = unsafe { fs_ext4_utimens(fs_handle, path_c.as_ptr(), a, 0, m, 0) };
    assert_eq!(rc, 0);
    unsafe { fs_ext4_umount(fs_handle) };

    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount failed — inode csum not patched?");
    let after = stat_attr(fs2, "/test.txt");
    // The attr struct widened to i64 in 0.6.0; the setter still takes
    // u32 seconds, so the comparison crosses the widening deliberately.
    assert_eq!(after.atime, a);
    assert_eq!(after.mtime, m);
    unsafe { fs_ext4_umount(fs2) };

    let _ = fs::remove_file(&img);
}

/// A timestamp past 2038 must come back as itself.
///
/// The seconds field is signed on disk, so anything at or above
/// 2^31 has to carry the epoch extension in the low two bits of the
/// matching `*_extra` field. A setter that writes the base and leaves
/// those bits zero stores a date in the 1900s instead.
#[test]
fn utimens_round_trips_a_date_past_2038() {
    let img = scratch("post_2038");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());

    // 2046-01-01. Fits in a u32; negative when read as an i32, so it
    // is stored as that negative base plus one epoch bit.
    let t = 2_398_291_200i64;
    let rc = unsafe { fs_ext4_utimens(fs_handle, path_c.as_ptr(), t, 0, t, 0) };
    assert_eq!(rc, 0, "utimens failed: {}", fs_ext4_last_errno());

    let after = stat_attr(fs_handle, "/test.txt");
    assert_eq!(after.atime, t, "atime lost its epoch extension");
    assert_eq!(after.mtime, t, "mtime lost its epoch extension");

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

/// A time ext4 cannot represent is refused, not silently truncated.
///
/// The on-disk encoding is a signed 32-bit base plus two epoch bits,
/// so roughly 1901-12-13 .. 2446-05-10. Now that the setter takes an
/// `i64`, a caller can name a time outside that; storing it would wrap
/// to some unrelated date.
#[test]
fn utimens_refuses_a_time_ext4_cannot_store() {
    let img = scratch("out_of_range");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());

    let before = stat_attr(fs_handle, "/test.txt");

    for (label, t) in [
        ("year 3000", 32_503_680_000i64),
        ("year 1800", -5_364_662_400i64),
    ] {
        let rc = unsafe { fs_ext4_utimens(fs_handle, path_c.as_ptr(), t, 0, t, 0) };
        assert_eq!(rc, -1, "{label} should be refused");
        assert_eq!(fs_ext4_last_errno(), 22, "{label} errno (EINVAL)");
    }

    // A refused call must leave the inode untouched.
    let after = stat_attr(fs_handle, "/test.txt");
    assert_eq!(after.atime, before.atime, "atime changed on a refused call");
    assert_eq!(after.mtime, before.mtime, "mtime changed on a refused call");
    assert_eq!(after.ctime, before.ctime, "ctime bumped on a refused call");

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

/// A pre-1970 time round-trips as a negative value.
///
/// The base is signed, so this needs no epoch bits at all — it just
/// needs the setter not to have taken a `u32`, which could not name it.
#[test]
fn utimens_round_trips_a_pre_1970_date() {
    let img = scratch("pre_1970");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());

    // 1960-01-01.
    let t = -315_619_200i64;
    let rc = unsafe { fs_ext4_utimens(fs_handle, path_c.as_ptr(), t, 0, t, 0) };
    assert_eq!(rc, 0, "utimens failed: {}", fs_ext4_last_errno());

    let after = stat_attr(fs_handle, "/test.txt");
    assert_eq!(after.atime, t, "a 1960 atime must stay in 1960");
    assert_eq!(after.mtime, t, "a 1960 mtime must stay in 1960");

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

/// The header's `FS_EXT4_UTIME_NOW` / `FS_EXT4_UTIME_OMIT`.
const C_UTIME_NOW: u32 = fs_ext4::fs::UTIME_NOW;
const C_UTIME_OMIT: u32 = fs_ext4::fs::UTIME_OMIT;

/// A nanosecond count of one billion or more is EINVAL, and the inode
/// is left exactly as it was (#326). Before, 1e9..2^30-1 was stored
/// verbatim and anything larger was masked into garbage.
#[test]
fn utimens_refuses_a_nanosecond_count_of_one_billion_or_more() {
    let img = scratch("nsec_range");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());

    let before = stat_attr(fs_handle, "/test.txt");
    for nsec in [1_000_000_000u32, 1 << 30, u32::MAX] {
        for (label, a, m) in [("atime", nsec, 0), ("mtime", 0, nsec)] {
            let rc = unsafe { fs_ext4_utimens(fs_handle, path_c.as_ptr(), 0, a, 0, m) };
            assert_eq!(rc, -1, "{label} nsec {nsec:#x} should be refused");
            assert_eq!(
                fs_ext4_last_errno(),
                22,
                "{label} nsec {nsec:#x} errno (EINVAL)"
            );
        }
    }
    let after = stat_attr(fs_handle, "/test.txt");
    assert_eq!(
        (
            after.atime,
            after.atime_nsec,
            after.mtime,
            after.mtime_nsec,
            after.ctime
        ),
        (
            before.atime,
            before.atime_nsec,
            before.mtime,
            before.mtime_nsec,
            before.ctime
        ),
        "a refused call changed the inode"
    );

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

/// `FS_EXT4_UTIME_NOW` in a nanoseconds field sets that timestamp to the
/// current time and ignores the seconds beside it.
#[test]
fn utimens_utime_now_sets_the_current_time() {
    let img = scratch("utime_now");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());

    let m = 946_771_200i64;
    let rc = unsafe { fs_ext4_utimens(fs_handle, path_c.as_ptr(), 0, C_UTIME_NOW, m, 5) };
    assert_eq!(rc, 0, "utimens failed: {}", fs_ext4_last_errno());

    let after = stat_attr(fs_handle, "/test.txt");
    // ctime is bumped to the same clock in the same call.
    assert_eq!(after.atime, after.ctime, "atime is not the current time");
    assert_eq!(after.atime_nsec, 0);
    assert_eq!((after.mtime, after.mtime_nsec), (m, 5), "mtime applied");

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

/// `FS_EXT4_UTIME_OMIT` in a nanoseconds field leaves that timestamp
/// alone and ignores the seconds beside it.
#[test]
fn utimens_utime_omit_leaves_the_field_alone() {
    let img = scratch("utime_omit");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());

    let before = stat_attr(fs_handle, "/test.txt");
    let a = 1_600_000_000i64;
    let rc = unsafe { fs_ext4_utimens(fs_handle, path_c.as_ptr(), a, 7, 0, C_UTIME_OMIT) };
    assert_eq!(rc, 0, "utimens failed: {}", fs_ext4_last_errno());

    let after = stat_attr(fs_handle, "/test.txt");
    assert_eq!((after.atime, after.atime_nsec), (a, 7), "atime applied");
    assert_eq!(
        (after.mtime, after.mtime_nsec),
        (before.mtime, before.mtime_nsec),
        "mtime preserved by UTIME_OMIT"
    );

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}
