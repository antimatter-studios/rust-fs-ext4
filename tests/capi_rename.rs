//! C-ABI tests for `fs_ext4_rename`. Covers intra-dir rename,
//! cross-dir move for files and directories (with `..` fix-up), and the
//! error paths we care about.

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

#[track_caller]
fn src_image() -> String {
    fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), "ext4-basic.img")
}

fn last_err_str() -> String {
    unsafe {
        let p = fs_ext4_last_error();
        if p.is_null() {
            return "<null>".into();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

fn scratch_image() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_rename_{}_{n}.img",
        std::process::id()
    ));
    let bytes = fs_ext4_test_support::read_small_file(src_image()).expect("read src image");
    let mut out = std::fs::File::create(&dst).expect("create dst image");
    out.write_all(&bytes).expect("write dst image");
    out.flush().expect("flush");
    drop(out);
    dst
}

fn path_exists(fs: *mut fs_ext4_fs_t, path: &str) -> bool {
    let p = CString::new(path).unwrap();
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    unsafe { fs_ext4_stat(fs, p.as_ptr(), &mut attr as *mut _) == 0 }
}

fn stat_ino(fs: *mut fs_ext4_fs_t, path: &str) -> u32 {
    let p = CString::new(path).unwrap();
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_ext4_stat(fs, p.as_ptr(), &mut attr as *mut _) };
    assert_eq!(rc, 0, "stat {path}: {}", last_err_str());
    attr.inode
}

fn rename(fs: *mut fs_ext4_fs_t, src: &str, dst: &str) -> i32 {
    let s = CString::new(src).unwrap();
    let d = CString::new(dst).unwrap();
    unsafe { fs_ext4_rename(fs, s.as_ptr(), d.as_ptr()) }
}

#[test]
fn intra_directory_rename_preserves_inode() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let ino_before = stat_ino(fs, "/test.txt");
    let rc = rename(fs, "/test.txt", "/test_renamed.txt");
    assert_eq!(rc, 0, "rename: {}", last_err_str());
    assert!(!path_exists(fs, "/test.txt"));
    let ino_after = stat_ino(fs, "/test_renamed.txt");
    assert_eq!(
        ino_after, ino_before,
        "rename should preserve the inode (it's a metadata-only op)"
    );
    unsafe { fs_ext4_umount(fs) };

    // Persists across remount.
    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount: {}", last_err_str());
    assert!(path_exists(fs2, "/test_renamed.txt"));
    assert!(!path_exists(fs2, "/test.txt"));
    unsafe { fs_ext4_umount(fs2) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn cross_parent_file_move() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let ino_before = stat_ino(fs, "/test.txt");
    let rc = rename(fs, "/test.txt", "/subdir/moved.txt");
    assert_eq!(rc, 0, "rename: {}", last_err_str());
    assert!(!path_exists(fs, "/test.txt"));
    let ino_after = stat_ino(fs, "/subdir/moved.txt");
    assert_eq!(ino_after, ino_before);
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn cross_parent_directory_move_updates_dotdot() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    // Build: /movable_dir/child.txt, then move movable_dir into subdir.
    let mdir = CString::new("/movable_dir").unwrap();
    let child = CString::new("/movable_dir/child.txt").unwrap();
    let d_ino = unsafe { fs_ext4_mkdir(fs, mdir.as_ptr(), 0o755) };
    assert!(d_ino > 0, "mkdir: {}", last_err_str());
    let c_ino = unsafe { fs_ext4_create(fs, child.as_ptr(), 0o644) };
    assert!(c_ino > 0, "create child: {}", last_err_str());

    let rc = rename(fs, "/movable_dir", "/subdir/movable_dir");
    assert_eq!(rc, 0, "cross-dir rename: {}", last_err_str());
    assert!(!path_exists(fs, "/movable_dir"));
    assert!(path_exists(fs, "/subdir/movable_dir"));
    assert!(
        path_exists(fs, "/subdir/movable_dir/child.txt"),
        "child must travel with the directory"
    );
    // The directory's inode number is preserved.
    assert_eq!(stat_ino(fs, "/subdir/movable_dir"), d_ino);
    unsafe { fs_ext4_umount(fs) };

    // Persists across remount + still reachable by new path.
    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount: {}", last_err_str());
    assert!(path_exists(fs2, "/subdir/movable_dir/child.txt"));
    unsafe { fs_ext4_umount(fs2) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn rename_refuses_when_destination_exists() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    // /test.txt and /subdir both exist on ext4-basic.img.
    let rc = rename(fs, "/test.txt", "/subdir");
    assert_eq!(rc, -1, "rename to existing dest must fail");
    let err = last_err_str();
    assert!(err.contains("exist"), "error should mention exists: {err}");
    assert!(path_exists(fs, "/test.txt"));
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn rename_same_src_and_dst_is_noop_success() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let rc = rename(fs, "/test.txt", "/test.txt");
    assert_eq!(rc, 0);
    assert!(path_exists(fs, "/test.txt"));
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn rename_refuses_directory_into_own_subtree() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    // /subdir exists. Try to move it into itself -> must fail.
    let rc = rename(fs, "/subdir", "/subdir/self");
    assert_eq!(rc, -1);
    assert!(path_exists(fs, "/subdir"));
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn rename_refuses_missing_source() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());
    let rc = rename(fs, "/no-such-file", "/also-no");
    assert_eq!(rc, -1);
    unsafe { fs_ext4_umount(fs) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn rename_refuses_on_ro_mount() {
    let img_c = CString::new(src_image()).unwrap();

    let fs = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount: {}", last_err_str());
    let rc = rename(fs, "/test.txt", "/renamed.txt");
    assert_eq!(rc, -1);
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn rename_null_inputs_do_not_crash() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null());

    let s = CString::new("/test.txt").unwrap();
    let d = CString::new("/renamed.txt").unwrap();
    assert_eq!(
        unsafe { fs_ext4_rename(std::ptr::null_mut(), s.as_ptr(), d.as_ptr()) },
        -1
    );
    assert_eq!(
        unsafe { fs_ext4_rename(fs, std::ptr::null(), d.as_ptr()) },
        -1
    );
    assert_eq!(
        unsafe { fs_ext4_rename(fs, s.as_ptr(), std::ptr::null()) },
        -1
    );

    unsafe { fs_ext4_umount(fs) };
    std::fs::remove_file(&img).ok();
}
