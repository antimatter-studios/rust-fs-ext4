//! C-ABI tests for `fs_ext4_rename2` with `FS_EXT4_RENAME_REPLACE`.
//! Covers the cases that Windows Explorer's "Save As" / drag-drop-
//! onto-existing depend on:
//!   - file replaces file (atomic, frees the old inode)
//!   - replace=false on existing dst returns EEXIST (regression guard)
//!   - empty-dir replaces empty-dir
//!   - non-empty-dir replace returns ENOTEMPTY
//!   - file → dir / dir → file return EISDIR / ENOTDIR
//!   - hardlinked dst preserves blocks (other link still resolves)
//!   - replace survives umount + RO remount

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::fs;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC_IMAGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

fn last_err_str() -> String {
    unsafe {
        let p = fs_ext4_last_error();
        if p.is_null() {
            return "<null>".into();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

fn scratch_image(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_rename_overwrite_{tag}_{}_{n}.img",
        std::process::id()
    ));
    fs::copy(SRC_IMAGE, &dst).expect("copy src image");
    dst
}

fn path_exists(fs: *mut fs_ext4_fs_t, path: &str) -> bool {
    let p = CString::new(path).unwrap();
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    unsafe { fs_ext4_stat(fs, p.as_ptr(), &mut attr as *mut _) == 0 }
}

fn stat_attr(fs: *mut fs_ext4_fs_t, path: &str) -> fs_ext4_attr_t {
    let p = CString::new(path).unwrap();
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_ext4_stat(fs, p.as_ptr(), &mut attr as *mut _) };
    assert_eq!(rc, 0, "stat {path}: {}", last_err_str());
    attr
}

fn create_file_with_content(fs: *mut fs_ext4_fs_t, path: &str, content: &[u8]) {
    let p = CString::new(path).unwrap();
    let ino = unsafe { fs_ext4_create(fs, p.as_ptr(), 0o644) };
    assert!(ino > 0, "create {path}: {}", last_err_str());
    if !content.is_empty() {
        let rc = unsafe {
            fs_ext4_write_file(
                fs,
                p.as_ptr(),
                content.as_ptr() as *const c_void,
                content.len() as u64,
            )
        };
        assert_eq!(rc, content.len() as i64, "write {path}: {}", last_err_str());
    }
}

fn read_full(fs: *mut fs_ext4_fs_t, path: &str) -> Vec<u8> {
    let attr = stat_attr(fs, path);
    let p = CString::new(path).unwrap();
    let mut buf = vec![0u8; attr.size as usize];
    let n = unsafe {
        fs_ext4_read_file(
            fs,
            p.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            0,
            buf.len() as u64,
        )
    };
    assert_eq!(n as usize, buf.len(), "read {path}: {}", last_err_str());
    buf
}

fn rename2(fs: *mut fs_ext4_fs_t, src: &str, dst: &str, flags: i32) -> i32 {
    let s = CString::new(src).unwrap();
    let d = CString::new(dst).unwrap();
    unsafe { fs_ext4_rename2(fs, s.as_ptr(), d.as_ptr(), flags) }
}

#[test]
fn file_replace_file_with_replace_flag_succeeds() {
    let img = scratch_image("file_repl_file");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    // /test.txt is the existing file on the basic image. Set up a fresh
    // src with distinctive content + a victim dst so we can verify the
    // overwrite landed atomically.
    create_file_with_content(fs, "/src.txt", b"src-content-AAA\n");
    create_file_with_content(fs, "/victim.txt", b"BBBBBBBBBBBBBBBBBBBBBBB\n");

    let src_ino = stat_attr(fs, "/src.txt").inode;
    let victim_ino = stat_attr(fs, "/victim.txt").inode;
    assert_ne!(src_ino, victim_ino);

    let rc = rename2(fs, "/src.txt", "/victim.txt", FS_EXT4_RENAME_REPLACE);
    assert_eq!(rc, 0, "rename2 replace: {}", last_err_str());

    // Source gone, dst now resolves to src's inode + content.
    assert!(!path_exists(fs, "/src.txt"));
    assert!(path_exists(fs, "/victim.txt"));
    assert_eq!(stat_attr(fs, "/victim.txt").inode, src_ino);
    assert_eq!(read_full(fs, "/victim.txt"), b"src-content-AAA\n");

    // The victim's old inode slot must be reclaimed — proven by creating
    // a new file and observing that the allocator can reuse the slot
    // (or at minimum that allocation still succeeds).
    create_file_with_content(fs, "/recycle.txt", b"x");
    assert!(path_exists(fs, "/recycle.txt"));

    unsafe { fs_ext4_umount(fs) };
    fs::remove_file(&img).ok();
}

#[test]
fn file_replace_file_without_replace_flag_returns_eexist() {
    let img = scratch_image("eexist");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    create_file_with_content(fs, "/src.txt", b"new\n");
    // /test.txt exists on the fixture.
    let rc = rename2(fs, "/src.txt", "/test.txt", 0);
    assert_eq!(rc, -1);
    assert_eq!(
        fs_ext4_last_errno(),
        17,
        "expected EEXIST: {}",
        last_err_str()
    );
    // Bare fs_ext4_rename also still returns EEXIST.
    let s = CString::new("/src.txt").unwrap();
    let d = CString::new("/test.txt").unwrap();
    assert_eq!(unsafe { fs_ext4_rename(fs, s.as_ptr(), d.as_ptr()) }, -1);
    assert_eq!(fs_ext4_last_errno(), 17);

    unsafe { fs_ext4_umount(fs) };
    fs::remove_file(&img).ok();
}

#[test]
fn empty_dir_replace_empty_dir_succeeds() {
    let img = scratch_image("dir_repl_dir");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let a = CString::new("/dir_a").unwrap();
    let b = CString::new("/dir_b").unwrap();
    let a_ino = unsafe { fs_ext4_mkdir(fs, a.as_ptr(), 0o755) };
    let b_ino = unsafe { fs_ext4_mkdir(fs, b.as_ptr(), 0o755) };
    assert!(a_ino > 0 && b_ino > 0);

    let rc = rename2(fs, "/dir_a", "/dir_b", FS_EXT4_RENAME_REPLACE);
    assert_eq!(rc, 0, "rename2 dir->dir: {}", last_err_str());
    assert!(!path_exists(fs, "/dir_a"));
    assert!(path_exists(fs, "/dir_b"));
    assert_eq!(stat_attr(fs, "/dir_b").inode, a_ino);

    unsafe { fs_ext4_umount(fs) };
    fs::remove_file(&img).ok();
}

#[test]
fn replace_non_empty_dir_returns_enotempty() {
    let img = scratch_image("non_empty");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let src = CString::new("/src_dir").unwrap();
    let _ = unsafe { fs_ext4_mkdir(fs, src.as_ptr(), 0o755) };

    let dst = CString::new("/dst_dir").unwrap();
    let _ = unsafe { fs_ext4_mkdir(fs, dst.as_ptr(), 0o755) };
    create_file_with_content(fs, "/dst_dir/inner.txt", b"keep me\n");

    let rc = rename2(fs, "/src_dir", "/dst_dir", FS_EXT4_RENAME_REPLACE);
    assert_eq!(rc, -1, "expected non-empty replace to fail");
    assert_eq!(
        fs_ext4_last_errno(),
        66,
        "expected ENOTEMPTY=66 on macOS: {}",
        last_err_str()
    );
    // Both paths still exist + the inner file is untouched.
    assert!(path_exists(fs, "/src_dir"));
    assert!(path_exists(fs, "/dst_dir"));
    assert!(path_exists(fs, "/dst_dir/inner.txt"));

    unsafe { fs_ext4_umount(fs) };
    fs::remove_file(&img).ok();
}

#[test]
fn file_replace_dir_returns_eisdir() {
    let img = scratch_image("file_repl_dir");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    create_file_with_content(fs, "/just_a_file.txt", b"x");
    let dst = CString::new("/some_dir").unwrap();
    let _ = unsafe { fs_ext4_mkdir(fs, dst.as_ptr(), 0o755) };

    let rc = rename2(fs, "/just_a_file.txt", "/some_dir", FS_EXT4_RENAME_REPLACE);
    assert_eq!(rc, -1);
    assert_eq!(
        fs_ext4_last_errno(),
        21,
        "expected EISDIR: {}",
        last_err_str()
    );
    assert!(path_exists(fs, "/just_a_file.txt"));
    assert!(path_exists(fs, "/some_dir"));

    unsafe { fs_ext4_umount(fs) };
    fs::remove_file(&img).ok();
}

#[test]
fn dir_replace_file_returns_enotdir() {
    let img = scratch_image("dir_repl_file");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let src = CString::new("/src_dir").unwrap();
    let _ = unsafe { fs_ext4_mkdir(fs, src.as_ptr(), 0o755) };
    create_file_with_content(fs, "/dst_file.txt", b"x");

    let rc = rename2(fs, "/src_dir", "/dst_file.txt", FS_EXT4_RENAME_REPLACE);
    assert_eq!(rc, -1);
    assert_eq!(
        fs_ext4_last_errno(),
        20,
        "expected ENOTDIR: {}",
        last_err_str()
    );
    assert!(path_exists(fs, "/src_dir"));
    assert!(path_exists(fs, "/dst_file.txt"));

    unsafe { fs_ext4_umount(fs) };
    fs::remove_file(&img).ok();
}

#[test]
fn hardlinked_dst_overwrite_preserves_other_link() {
    let img = scratch_image("hardlinked");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    // Build a hardlinked target — /dst.txt and /dst_alt.txt share an
    // inode with link_count = 2.
    create_file_with_content(fs, "/dst.txt", b"shared content\n");
    let s = CString::new("/dst.txt").unwrap();
    let d = CString::new("/dst_alt.txt").unwrap();
    let lrc = unsafe { fs_ext4_link(fs, s.as_ptr(), d.as_ptr()) };
    assert_eq!(lrc, 0, "link: {}", last_err_str());
    let dst_ino_before = stat_attr(fs, "/dst.txt").inode;
    assert_eq!(stat_attr(fs, "/dst_alt.txt").inode, dst_ino_before);
    assert_eq!(stat_attr(fs, "/dst.txt").link_count, 2);

    create_file_with_content(fs, "/replacement.txt", b"NEW NEW NEW\n");
    let new_ino = stat_attr(fs, "/replacement.txt").inode;
    assert_ne!(new_ino, dst_ino_before);

    let rc = rename2(fs, "/replacement.txt", "/dst.txt", FS_EXT4_RENAME_REPLACE);
    assert_eq!(rc, 0, "rename2: {}", last_err_str());

    // /dst.txt now points to the replacement inode + content.
    assert_eq!(stat_attr(fs, "/dst.txt").inode, new_ino);
    assert_eq!(read_full(fs, "/dst.txt"), b"NEW NEW NEW\n");

    // /dst_alt.txt still resolves the OLD content (via the surviving
    // hardlink) — its link_count must have dropped from 2 to 1.
    assert!(path_exists(fs, "/dst_alt.txt"));
    assert_eq!(stat_attr(fs, "/dst_alt.txt").inode, dst_ino_before);
    assert_eq!(read_full(fs, "/dst_alt.txt"), b"shared content\n");
    assert_eq!(stat_attr(fs, "/dst_alt.txt").link_count, 1);

    unsafe { fs_ext4_umount(fs) };
    fs::remove_file(&img).ok();
}

#[test]
fn replace_persists_across_remount() {
    let img = scratch_image("persist");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    // RW phase: build src+victim, replace, unmount.
    {
        let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
        assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
        create_file_with_content(fs, "/src.txt", b"final-payload-XYZ\n");
        create_file_with_content(fs, "/victim.txt", b"will-be-overwritten\n");
        let rc = rename2(fs, "/src.txt", "/victim.txt", FS_EXT4_RENAME_REPLACE);
        assert_eq!(rc, 0, "rename2 replace: {}", last_err_str());
        unsafe { fs_ext4_umount(fs) };
    }

    // RO remount: rename must still resolve correctly.
    {
        let fs = unsafe { fs_ext4_mount(img_c.as_ptr()) };
        assert!(!fs.is_null(), "ro remount: {}", last_err_str());
        assert!(!path_exists(fs, "/src.txt"));
        assert!(path_exists(fs, "/victim.txt"));
        assert_eq!(read_full(fs, "/victim.txt"), b"final-payload-XYZ\n");
        unsafe { fs_ext4_umount(fs) };
    }

    fs::remove_file(&img).ok();
}

// ---------------------------------------------------------------------------
// Parent link counts across a rename
//
// A directory's `i_links_count` counts its `..` back-references: one per
// immediate subdirectory, plus its own `.` and its entry in its parent.
// Moving a directory between parents therefore moves one count, and
// replacing a directory with another destroys one and creates one.
//
// These were uncovered. The link-count patches used to be applied where
// each was discovered, reading the parent inode back from disk each
// time — so two patches naming the same inode in one transaction would
// have had the second overwrite the first. The branch that would have
// done it was suppressed by hand, in a condition that had to be kept in
// step with the branch it was suppressing.
// ---------------------------------------------------------------------------

fn mkdir(fs: *mut fs_ext4_fs_t, path: &str) {
    let p = CString::new(path).unwrap();
    let ino = unsafe { fs_ext4_mkdir(fs, p.as_ptr(), 0o755) };
    assert!(ino > 0, "mkdir {path}: {}", last_err_str());
}

fn links(fs: *mut fs_ext4_fs_t, path: &str) -> u16 {
    stat_attr(fs, path).link_count
}

/// Moving a directory to a different parent moves one link with it.
#[test]
fn a_cross_parent_directory_move_moves_one_parent_link() {
    let img = scratch_image("xparent_dir_move");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    mkdir(fs, "/from");
    mkdir(fs, "/to");
    mkdir(fs, "/from/sub");

    let from_before = links(fs, "/from");
    let to_before = links(fs, "/to");

    let rc = rename2(fs, "/from/sub", "/to/sub", 0);
    assert_eq!(rc, 0, "rename2: {}", last_err_str());

    assert_eq!(
        links(fs, "/from"),
        from_before - 1,
        "the old parent lost a subdirectory"
    );
    assert_eq!(links(fs, "/to"), to_before + 1, "the new parent gained one");
    assert!(path_exists(fs, "/to/sub"));

    unsafe { fs_ext4_umount(fs) };
    fs::remove_file(&img).ok();
}

/// Replacing a directory within one parent loses exactly one link.
#[test]
fn a_same_parent_directory_replace_loses_one_parent_link() {
    let img = scratch_image("same_parent_dir_replace");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    mkdir(fs, "/holder");
    mkdir(fs, "/holder/a");
    mkdir(fs, "/holder/b");

    let before = links(fs, "/holder");

    let rc = rename2(fs, "/holder/a", "/holder/b", FS_EXT4_RENAME_REPLACE);
    assert_eq!(rc, 0, "rename2 replace: {}", last_err_str());

    assert_eq!(
        links(fs, "/holder"),
        before - 1,
        "two subdirectories became one"
    );
    assert!(!path_exists(fs, "/holder/a"));
    assert!(path_exists(fs, "/holder/b"));

    unsafe { fs_ext4_umount(fs) };
    fs::remove_file(&img).ok();
}

/// The case where two deltas name the same inode.
///
/// A directory moves from one parent into another, replacing a
/// directory there: the destination parent gains the arriving
/// subdirectory (+1) and loses the departing one (-1). They must cancel
/// — and they are discovered in two different branches, hundreds of
/// lines apart, which is what made a hand-suppressed `+1` fragile.
#[test]
fn a_cross_parent_directory_replace_leaves_the_destination_parent_unchanged() {
    let img = scratch_image("xparent_dir_replace");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    mkdir(fs, "/left");
    mkdir(fs, "/right");
    mkdir(fs, "/left/moving");
    mkdir(fs, "/right/doomed");

    let left_before = links(fs, "/left");
    let right_before = links(fs, "/right");

    let rc = rename2(fs, "/left/moving", "/right/doomed", FS_EXT4_RENAME_REPLACE);
    assert_eq!(rc, 0, "rename2 replace: {}", last_err_str());

    assert_eq!(
        links(fs, "/left"),
        left_before - 1,
        "the source parent lost its subdirectory"
    );
    assert_eq!(
        links(fs, "/right"),
        right_before,
        "the destination parent gained one and lost one — net zero"
    );
    assert!(path_exists(fs, "/right/doomed"));
    assert!(!path_exists(fs, "/left/moving"));

    unsafe { fs_ext4_umount(fs) };
    fs::remove_file(&img).ok();
}

/// The counts must survive the journal, not just the in-memory view.
#[test]
fn parent_link_counts_survive_a_remount() {
    let img = scratch_image("nlink_remount");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    {
        let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
        assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
        mkdir(fs, "/src");
        mkdir(fs, "/dst");
        mkdir(fs, "/src/kid");
        let rc = rename2(fs, "/src/kid", "/dst/kid", 0);
        assert_eq!(rc, 0, "rename2: {}", last_err_str());
        unsafe { fs_ext4_umount(fs) };
    }

    {
        let fs = unsafe { fs_ext4_mount(img_c.as_ptr()) };
        assert!(!fs.is_null(), "ro remount: {}", last_err_str());
        // 2 for `.` and the entry in `/`, plus one per subdirectory.
        assert_eq!(links(fs, "/src"), 2, "/src has no subdirectories left");
        assert_eq!(links(fs, "/dst"), 3, "/dst has one");
        unsafe { fs_ext4_umount(fs) };
    }

    fs::remove_file(&img).ok();
}
