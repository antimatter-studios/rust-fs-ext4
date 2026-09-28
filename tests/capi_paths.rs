//! Path edge-case coverage for the C ABI.
//!
//! POSIX resolution rules:
//!   - "" / "/" / "//" all mean root (inode 2)
//!   - "//test.txt" equivalent to "/test.txt" (internal doubled slashes collapsed)
//!   - "/subdir/" equivalent to "/subdir" (trailing slash OK on directories)
//!   - "/test.txt/" must fail with ENOTDIR (trailing slash on non-dir is a
//!     POSIX violation — e.g. `rm /test.txt/` must not succeed even though
//!     /test.txt exists, because the path explicitly asked for a directory).

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};

fn last_err() -> String {
    unsafe {
        let p = fs_ext4_last_error();
        if p.is_null() {
            return String::new();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

#[track_caller]
fn mount_fixture() -> *mut fs_ext4_fs_t {
    let image = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), "ext4-basic.img");
    let p = CString::new(image.as_str()).unwrap();
    let fs = unsafe { fs_ext4_mount(p.as_ptr()) };
    assert!(!fs.is_null(), "mount {image}: {}", last_err());
    fs
}

fn stat_ino(fs: *mut fs_ext4_fs_t, path: &str) -> Option<u32> {
    let c = CString::new(path).unwrap();
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_ext4_stat(fs, c.as_ptr(), &mut attr) };
    if rc == 0 {
        Some(attr.inode)
    } else {
        None
    }
}

#[test]
fn empty_slash_and_double_slash_all_resolve_to_root() {
    let fs = mount_fixture();
    let by_slash = stat_ino(fs, "/").expect("/");
    let by_empty = stat_ino(fs, "").expect("empty");
    let by_double = stat_ino(fs, "//").expect("//");
    assert_eq!(by_slash, 2, "/ should resolve to root inode 2");
    assert_eq!(by_empty, 2, "empty string should resolve to root");
    assert_eq!(by_double, 2, "// should resolve to root");
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn trailing_slash_on_regular_file_yields_enotdir() {
    // POSIX: `foo/` only valid when foo is a directory. `/test.txt/` must
    // fail with ENOTDIR. Previously accepted as equivalent to `/test.txt`
    // which allowed `unlink("/test.txt/")` to succeed — bug flagged by @4.
    let fs = mount_fixture();
    let ino = stat_ino(fs, "/test.txt/");
    assert!(ino.is_none(), "/test.txt/ must not resolve as a directory");
    assert_eq!(fs_ext4_last_errno(), 20, "ENOTDIR expected"); // ENOTDIR
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn doubled_slashes_in_path_are_tolerated() {
    let fs = mount_fixture();
    let plain = stat_ino(fs, "/test.txt").expect("/test.txt");
    let doubled = stat_ino(fs, "//test.txt").expect("//test.txt");
    assert_eq!(plain, doubled);
    let tripled = stat_ino(fs, "///test.txt").expect("///test.txt");
    assert_eq!(plain, tripled);
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn path_through_subdir_with_trailing_slash() {
    let fs = mount_fixture();
    let plain = stat_ino(fs, "/subdir").expect("/subdir");
    let trailing = stat_ino(fs, "/subdir/").expect("/subdir/");
    assert_eq!(plain, trailing);
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn nonexistent_component_yields_enoent() {
    let fs = mount_fixture();
    let ino = stat_ino(fs, "/does-not-exist-at-all");
    assert!(ino.is_none());
    assert_eq!(fs_ext4_last_errno(), 2); // ENOENT
    assert!(!last_err().is_empty());
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn file_used_as_directory_mid_path_yields_enotdir() {
    let fs = mount_fixture();
    // /test.txt is a file; treating it as a directory must fail with ENOTDIR.
    let ino = stat_ino(fs, "/test.txt/anything");
    assert!(ino.is_none());
    assert_eq!(fs_ext4_last_errno(), 20); // ENOTDIR
    unsafe { fs_ext4_umount(fs) };
}

// ---- paths are bytes (#418) --------------------------------------------

/// `/caf\xe9.txt` — latin-1 for `café.txt`, which is what a name written
/// on a Linux box with a non-UTF-8 locale looks like. `\xe9` alone is not
/// a legal UTF-8 sequence.
///
/// `c_char` is `i8` on x86_64 and Apple targets and `u8` on
/// aarch64-linux, so `from_ne_bytes` is the spelling that works on both.
fn non_utf8_path() -> Vec<std::ffi::c_char> {
    b"/caf\xe9.txt\0"
        .iter()
        .map(|&b| std::ffi::c_char::from_ne_bytes([b]))
        .collect()
}

/// A path whose bytes are not valid UTF-8 names no file in this fixture,
/// so it must be reported as missing — never as the root.
///
/// `cstr_to_str` answered `""` for anything that did not decode, and
/// **`""` is a legitimate spelling of the root here** — see
/// `empty_slash_and_double_slash_all_resolve_to_root`, which is
/// deliberate and stays. So an undecodable path was silently converted
/// into the one input that means "the root", and `fs_ext4_stat` filled
/// the attribute struct with inode 2 and returned 0, indistinguishable
/// from a real hit (#418).
///
/// Reachable rather than theoretical: ext4 directory entry names are raw
/// bytes with no encoding rule, so a caller composing a path from a name
/// this driver handed it got the root back.
#[test]
fn a_non_utf8_path_that_names_no_file_is_not_the_root() {
    let fs = mount_fixture();
    let path = non_utf8_path();
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_ext4_stat(fs, path.as_ptr(), &mut attr) };
    assert_eq!(rc, -1, "a path naming no file was answered as a stat");
    // The mode, not the inode number: a refusal leaves `attr` zeroed, and
    // the root's mode is the non-zero one a root answer would write.
    assert_eq!(
        attr.mode, 0,
        "the root's attributes were reported for a path that names no file"
    );
    // A path is bytes, compared byte for byte (#418): one that names no
    // file is missing, which is ENOENT — not EINVAL, which would say the
    // bytes themselves were unacceptable.
    assert_eq!(
        fs_ext4_last_errno(),
        2,
        "a path naming no file must be ENOENT: {}",
        last_err()
    );
    unsafe { fs_ext4_umount(fs) };
}

/// The directory iterator likewise: this is the one that hurts most,
/// because a caller walking a tree and composing paths from the names
/// this driver handed back gets the root's entries again, and walks in a
/// circle.
#[test]
fn dir_open_on_a_non_utf8_path_that_names_nothing_is_not_the_root_listing() {
    let fs = mount_fixture();
    let path = non_utf8_path();
    let iter = unsafe { fs_ext4_dir_open(fs, path.as_ptr()) };
    assert!(
        iter.is_null(),
        "a path naming no directory opened an iterator over the root"
    );
    unsafe { fs_ext4_umount(fs) };
}

/// And the empty path still means the root, because that is deliberate
/// and documented at the top of this file. Without this the fix above
/// could be "refuse anything that does not decode", which would break
/// the POSIX-ish spelling the other tests rely on.
#[test]
fn the_empty_path_still_means_the_root() {
    let fs = mount_fixture();
    assert_eq!(stat_ino(fs, ""), Some(2));
    unsafe { fs_ext4_umount(fs) };
}
