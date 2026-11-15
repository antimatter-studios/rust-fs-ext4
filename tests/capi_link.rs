//! C-ABI tests for `fs_ext4_link`: creates hard links and — by pairing
//! with `fs_ext4_unlink` — exercises the nlink > 1 branch of unlink
//! that was previously only implicitly covered.

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::io::Write;
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

fn scratch_image() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_link_{}_{n}.img",
        std::process::id()
    ));
    let bytes = fs_ext4_test_support::read_small_file(SRC_IMAGE).expect("read src image");
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

fn stat(fs: *mut fs_ext4_fs_t, path: &str) -> fs_ext4_attr_t {
    let p = CString::new(path).unwrap();
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_ext4_stat(fs, p.as_ptr(), &mut attr as *mut _) };
    assert_eq!(rc, 0, "stat {path}: {}", last_err_str());
    attr
}

fn link(fs: *mut fs_ext4_fs_t, src: &str, dst: &str) -> i32 {
    let s = CString::new(src).unwrap();
    let d = CString::new(dst).unwrap();
    unsafe { fs_ext4_link(fs, s.as_ptr(), d.as_ptr()) }
}

#[test]
fn link_creates_second_name_for_same_inode() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let before = stat(fs, "/test.txt");
    let before_nlink = before.link_count;

    let rc = link(fs, "/test.txt", "/aka.txt");
    assert_eq!(rc, 0, "link: {}", last_err_str());

    // Both names resolve to the same inode.
    let a = stat(fs, "/test.txt");
    let b = stat(fs, "/aka.txt");
    assert_eq!(a.inode, b.inode, "hardlinked paths must share inode");
    assert_eq!(a.inode, before.inode);
    assert_eq!(a.link_count, before_nlink + 1, "nlink must be incremented");
    assert_eq!(b.link_count, a.link_count);

    unsafe { fs_ext4_umount(fs) };

    // Persists across remount.
    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount: {}", last_err_str());
    assert!(path_exists(fs2, "/test.txt"));
    assert!(path_exists(fs2, "/aka.txt"));
    assert_eq!(stat(fs2, "/test.txt").inode, stat(fs2, "/aka.txt").inode);
    unsafe { fs_ext4_umount(fs2) };

    std::fs::remove_file(&img).ok();
}

#[test]
fn unlink_first_hardlink_keeps_content_via_second_name() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    // Create a hardlink, then unlink the original; the content must
    // remain reachable via the second name with nlink decremented to 1.
    assert_eq!(
        link(fs, "/test.txt", "/aka.txt"),
        0,
        "link: {}",
        last_err_str()
    );
    let original = stat(fs, "/test.txt");

    let p = CString::new("/test.txt").unwrap();
    let rc = unsafe { fs_ext4_unlink(fs, p.as_ptr()) };
    assert_eq!(rc, 0, "unlink primary: {}", last_err_str());
    assert!(!path_exists(fs, "/test.txt"));
    assert!(
        path_exists(fs, "/aka.txt"),
        "hardlink must survive primary unlink"
    );

    let survivor = stat(fs, "/aka.txt");
    assert_eq!(survivor.inode, original.inode);
    assert_eq!(
        survivor.link_count,
        original.link_count - 1,
        "nlink should drop by 1 on unlink of one link"
    );

    // Read content via the survivor path — the inode's data blocks must
    // still be allocated.
    let mut buf = vec![0u8; survivor.size as usize];
    let aka = CString::new("/aka.txt").unwrap();
    let n = unsafe {
        fs_ext4_read_file(
            fs,
            aka.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            0,
            buf.len() as u64,
        )
    };
    assert!(n > 0, "read survivor: {}", last_err_str());

    unsafe { fs_ext4_umount(fs) };
    std::fs::remove_file(&img).ok();
}

#[test]
fn unlink_last_hardlink_frees_inode() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    // Create link → unlink both names → content gone, inode freed.
    assert_eq!(link(fs, "/test.txt", "/aka.txt"), 0);
    let p1 = CString::new("/test.txt").unwrap();
    let p2 = CString::new("/aka.txt").unwrap();
    assert_eq!(unsafe { fs_ext4_unlink(fs, p1.as_ptr()) }, 0);
    assert_eq!(unsafe { fs_ext4_unlink(fs, p2.as_ptr()) }, 0);
    assert!(!path_exists(fs, "/test.txt"));
    assert!(!path_exists(fs, "/aka.txt"));

    unsafe { fs_ext4_umount(fs) };
    std::fs::remove_file(&img).ok();
}

#[test]
fn link_refuses_directory_source() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let rc = link(fs, "/subdir", "/subdir_alias");
    assert_eq!(rc, -1, "hardlinks to directories must fail");
    let err = last_err_str();
    assert!(err.contains("director"), "error: {err}");
    assert!(!path_exists(fs, "/subdir_alias"));

    unsafe { fs_ext4_umount(fs) };
    std::fs::remove_file(&img).ok();
}

#[test]
fn link_refuses_existing_destination() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    // /subdir already exists as a dir.
    let rc = link(fs, "/test.txt", "/subdir");
    assert_eq!(rc, -1);

    unsafe { fs_ext4_umount(fs) };
    std::fs::remove_file(&img).ok();
}

#[test]
fn link_refuses_missing_source() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let rc = link(fs, "/nope", "/also-nope");
    assert_eq!(rc, -1);

    unsafe { fs_ext4_umount(fs) };
    std::fs::remove_file(&img).ok();
}

#[test]
fn link_refuses_on_ro_mount() {
    let img_c = CString::new(SRC_IMAGE).unwrap();

    let fs = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount: {}", last_err_str());
    let rc = link(fs, "/test.txt", "/would_be_link.txt");
    assert_eq!(rc, -1);
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn link_null_inputs_do_not_crash() {
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null());

    let s = CString::new("/test.txt").unwrap();
    let d = CString::new("/link.txt").unwrap();
    assert_eq!(
        unsafe { fs_ext4_link(std::ptr::null_mut(), s.as_ptr(), d.as_ptr()) },
        -1
    );
    assert_eq!(
        unsafe { fs_ext4_link(fs, std::ptr::null(), d.as_ptr()) },
        -1
    );
    assert_eq!(
        unsafe { fs_ext4_link(fs, s.as_ptr(), std::ptr::null()) },
        -1
    );

    unsafe { fs_ext4_umount(fs) };
    std::fs::remove_file(&img).ok();
}

#[test]
fn can_chain_many_hardlinks() {
    // Stress the nlink increment path: create 10 hardlinks, confirm
    // link_count tracks correctly, unlink them all, confirm cleanup.
    let img = scratch_image();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let fs = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err_str());

    let orig_nlink = stat(fs, "/test.txt").link_count;
    for i in 0..10 {
        let dst = format!("/link_{i}.txt");
        assert_eq!(
            link(fs, "/test.txt", &dst),
            0,
            "link {dst}: {}",
            last_err_str()
        );
    }
    assert_eq!(stat(fs, "/test.txt").link_count, orig_nlink + 10);

    for i in 0..10 {
        let dst = format!("/link_{i}.txt");
        let p = CString::new(dst.clone()).unwrap();
        assert_eq!(unsafe { fs_ext4_unlink(fs, p.as_ptr()) }, 0, "unlink {dst}");
    }
    assert_eq!(stat(fs, "/test.txt").link_count, orig_nlink);

    unsafe { fs_ext4_umount(fs) };
    std::fs::remove_file(&img).ok();
}
