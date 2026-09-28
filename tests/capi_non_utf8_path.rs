//! Paths cross the C ABI as bytes (#418).
//!
//! ext4 directory entry names are raw bytes: no NUL, no `/`, and no
//! encoding rule — the format has no field that could say what encoding a
//! name is in. Any image built on a box with a non-UTF-8 locale holds such
//! names, as does anything copied off a legacy volume. So every
//! path-taking entry point reads its `const char *` as the bytes up to the
//! NUL and compares them byte for byte against the names in the image.
//!
//! What this replaced: `cstr_to_str` decoded the path as UTF-8 and
//! answered `""` for anything that did not decode — and `""` is a
//! deliberate spelling of the root here. A non-UTF-8 path was therefore
//! answered as a successful stat *of the root*, and a caller composing a
//! path from a name `fs_ext4_dir_next` had just handed it walked in a
//! circle. The first test in this file used to tolerate exactly that
//! ("Non-UTF-8 interpreted as empty → root (inode 2). Tolerable."); it now
//! asserts the contract instead.
//!
//! These tests create the names through the `(directory, name)` entry
//! points, which have taken names as counted bytes since #419 and share
//! none of the path parsing, and then reach them by path. The kernel-side
//! half — a name `debugfs` wrote, reached by its bytes and judged by
//! `e2fsck` afterwards — is `tests/non_utf8_names_oracle.rs`.

use fs_ext4::capi::*;
use std::ffi::CStr;
use std::fs;
use std::io::Write;
use std::os::raw::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

const ENOENT: i32 = 2;
const EINVAL: i32 = 22;
const ROOT: u32 = 2;

/// `café.txt` as a latin-1 machine writes it: `\xe9` alone is not UTF-8.
const CAFE: &[u8] = b"caf\xe9.txt";
/// A directory whose name is a byte no UTF-8 string can hold.
const DIR: &[u8] = b"d\xff";

fn scratch() -> PathBuf {
    static C: AtomicU32 = AtomicU32::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(fs_ext4_test_support::temp_path!(
        "fs_ext4_capi_nonutf8_{}_{n}.img",
        std::process::id()
    ));
    let mut out = fs::File::create(&dst).unwrap();
    out.write_all(
        &fs::read(fs_ext4_test_support::fixture(
            env!("CARGO_MANIFEST_DIR"),
            "ext4-basic.img",
        ))
        .unwrap(),
    )
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

/// `bytes` as a NUL-terminated C string. `c_char` is signed on x86_64 and
/// Apple targets and unsigned on aarch64-linux; `from_ne_bytes` is the
/// spelling that compiles on both.
fn c(bytes: &[u8]) -> Vec<c_char> {
    assert!(!bytes.contains(&0), "a C path cannot hold a NUL");
    bytes
        .iter()
        .chain(std::iter::once(&0))
        .map(|&b| c_char::from_ne_bytes([b]))
        .collect()
}

/// `/` followed by each component, joined with `/`.
fn path(components: &[&[u8]]) -> Vec<u8> {
    let mut p = Vec::new();
    for component in components {
        p.push(b'/');
        p.extend_from_slice(component);
    }
    p
}

fn mount_rw(img: &Path) -> *mut fs_ext4_fs_t {
    let p = c(img.to_str().unwrap().as_bytes());
    let fs = unsafe { fs_ext4_mount_rw(p.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_err());
    fs
}

/// Stat `p` by path; the attributes, or the errno.
fn stat(fs: *mut fs_ext4_fs_t, p: &[u8]) -> Result<fs_ext4_attr_t, i32> {
    let p = c(p);
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    if unsafe { fs_ext4_stat(fs, p.as_ptr(), &mut attr) } == 0 {
        Ok(attr)
    } else {
        assert_eq!(attr.mode, 0, "a failed stat wrote attributes");
        Err(fs_ext4_last_errno())
    }
}

/// What `name` names in directory `dir`, found by the byte-name entry
/// point rather than by path: the independent check on every path call.
fn lookup_at(fs: *mut fs_ext4_fs_t, dir: u32, name: &[u8]) -> Option<u32> {
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        fs_ext4_lookup_at(
            fs,
            dir,
            FS_EXT4_GEN_ANY,
            name.as_ptr().cast(),
            name.len(),
            &mut attr,
        )
    };
    (rc == 0).then_some(attr.inode)
}

/// Every name `fs_ext4_dir_open` lists for path `p`, as the bytes it hands
/// the caller.
fn list(fs: *mut fs_ext4_fs_t, p: &[u8]) -> Vec<(Vec<u8>, u32)> {
    let cp = c(p);
    let iter = unsafe { fs_ext4_dir_open(fs, cp.as_ptr()) };
    assert!(
        !iter.is_null(),
        "dir_open {:?}: {}",
        String::from_utf8_lossy(p),
        last_err()
    );
    let mut out = Vec::new();
    loop {
        let d = unsafe { fs_ext4_dir_next(iter) };
        if d.is_null() {
            break;
        }
        let d = unsafe { &*d };
        let name: Vec<u8> = d.name[..d.name_len as usize]
            .iter()
            .map(|&b| b.to_ne_bytes()[0])
            .collect();
        out.push((name, d.inode));
    }
    unsafe { fs_ext4_dir_close(iter) };
    out
}

fn read_all(fs: *mut fs_ext4_fs_t, p: &[u8], len: usize) -> Vec<u8> {
    let cp = c(p);
    let mut buf = vec![0u8; len + 16];
    let n = unsafe {
        fs_ext4_read_file(
            fs,
            cp.as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            buf.len() as u64,
        )
    };
    assert!(
        n >= 0,
        "read_file {:?}: {}",
        String::from_utf8_lossy(p),
        last_err()
    );
    buf.truncate(n as usize);
    buf
}

/// A path whose bytes are not UTF-8 and name no file is ENOENT, and the
/// attributes are left alone.
///
/// THIS TEST USED TO ACCEPT THE DEFECT. As `stat_on_non_utf8_path_does_not_crash`
/// it allowed either outcome, and for success asserted `attr.inode == 2` —
/// the root — under the comment "Tolerable". It was the #418 bug written
/// down as a contract.
#[test]
fn stat_on_a_non_utf8_path_naming_nothing_is_enoent_never_the_root() {
    let img = scratch();
    let fs = mount_rw(&img);
    assert_eq!(
        stat(fs, b"\xff\xfe").map(|a| a.inode),
        Err(ENOENT),
        "{}",
        last_err()
    );
    assert_eq!(
        stat(fs, &path(&[CAFE])).map(|a| a.inode),
        Err(ENOENT),
        "{}",
        last_err()
    );
    unsafe { fs_ext4_umount(fs) };
    let _ = fs::remove_file(&img);
}

/// A relative path is refused for a create, whatever its bytes, and the
/// volume is untouched.
#[test]
fn create_on_a_relative_non_utf8_path_is_refused_and_changes_nothing() {
    let img = scratch();
    let fs = mount_rw(&img);
    let root_before = list(fs, b"/");
    let bad = c(b"\xff\xfe");
    let ino = unsafe { fs_ext4_create(fs, bad.as_ptr(), 0o644) };
    assert_eq!(ino, 0, "create on a relative path made inode {ino}");
    assert_eq!(fs_ext4_last_errno(), EINVAL, "{}", last_err());
    assert_eq!(list(fs, b"/"), root_before, "the root listing changed");
    unsafe { fs_ext4_umount(fs) };
    let _ = fs::remove_file(&img);
}

#[test]
fn unlink_on_a_non_utf8_path_naming_nothing_is_enoent() {
    let img = scratch();
    let fs = mount_rw(&img);
    let bad = c(&path(&[b"\xff\xfe"]));
    let rc = unsafe { fs_ext4_unlink(fs, bad.as_ptr()) };
    assert_eq!(rc, -1, "unlink of a path naming nothing succeeded");
    assert_eq!(fs_ext4_last_errno(), ENOENT, "{}", last_err());
    unsafe { fs_ext4_umount(fs) };
    let _ = fs::remove_file(&img);
}

/// The issue's "done when": a name this library lists is handed straight
/// back, by path, to `stat`, `read_file`, `dir_open` and `readlink`, and
/// each answers about that file.
///
/// The names are made through the `(directory, name)` entry points, which
/// take counted bytes and resolve no path, so the path half under test
/// here does not also build its own fixture.
#[test]
fn a_name_dir_next_listed_is_reachable_by_its_bytes() {
    let img = scratch();
    let fs = mount_rw(&img);
    let payload = b"reached by its bytes\n";
    let target: &[u8] = b"t\xfe\xff";

    let mut file: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let mut dir: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let target_c = c(target);
    unsafe {
        let rc = fs_ext4_create_at(
            fs,
            ROOT,
            FS_EXT4_GEN_ANY,
            CAFE.as_ptr().cast(),
            CAFE.len(),
            0o644,
            &mut file,
        );
        assert_eq!(rc, 0, "create_at: {}", last_err());
        let n = fs_ext4_pwrite_ino(
            fs,
            file.inode,
            FS_EXT4_GEN_ANY,
            payload.as_ptr().cast(),
            payload.len() as u64,
            0,
        );
        assert_eq!(n, payload.len() as i64, "pwrite_ino: {}", last_err());
        let rc = fs_ext4_mkdir_at(
            fs,
            ROOT,
            FS_EXT4_GEN_ANY,
            DIR.as_ptr().cast(),
            DIR.len(),
            0o755,
            &mut dir,
        );
        assert_eq!(rc, 0, "mkdir_at: {}", last_err());
        let link = b"s\xe9";
        let rc = fs_ext4_symlink_at(
            fs,
            dir.inode,
            FS_EXT4_GEN_ANY,
            link.as_ptr().cast(),
            link.len(),
            target_c.as_ptr(),
            std::ptr::null_mut(),
        );
        assert_eq!(rc, 0, "symlink_at: {}", last_err());
    }

    // Walk the tree the way a host does: list, then hand each listed name
    // back as a path.
    let root = list(fs, b"/");
    let listed_file = root
        .iter()
        .find(|(n, _)| n == CAFE)
        .expect("dir_next lists the non-UTF-8 file name byte for byte");
    let listed_dir = root
        .iter()
        .find(|(n, _)| n == DIR)
        .expect("dir_next lists the non-UTF-8 directory name byte for byte");

    let by_path = stat(fs, &path(&[&listed_file.0]));
    assert_eq!(
        by_path.map(|a| a.inode),
        Ok(file.inode),
        "stat of a name dir_next listed: {}",
        last_err()
    );
    assert_eq!(listed_file.1, file.inode);
    assert_eq!(
        read_all(fs, &path(&[&listed_file.0]), payload.len()),
        payload,
        "read_file of a name dir_next listed"
    );

    let inside = list(fs, &path(&[&listed_dir.0]));
    let (link_name, link_ino) = inside
        .iter()
        .find(|(n, _)| n.as_slice() == b"s\xe9")
        .expect("dir_open of a non-UTF-8 directory lists its symlink")
        .clone();
    let link_path = path(&[&listed_dir.0, &link_name]);
    assert_eq!(stat(fs, &link_path).map(|a| a.inode), Ok(link_ino));
    let mut buf = [0 as c_char; 64];
    let lp = c(&link_path);
    let n = unsafe { fs_ext4_readlink(fs, lp.as_ptr(), buf.as_mut_ptr(), buf.len()) };
    assert_eq!(n, target.len() as i32, "readlink: {}", last_err());
    let got: Vec<u8> = buf[..n as usize]
        .iter()
        .map(|&b| b.to_ne_bytes()[0])
        .collect();
    assert_eq!(got, target, "readlink handed back other bytes");

    // Neighbouring bytes are different names: `\xea` is not `\xe9`.
    assert_eq!(stat(fs, b"/caf\xea.txt").map(|a| a.inode), Err(ENOENT));

    unsafe { fs_ext4_umount(fs) };
    let _ = fs::remove_file(&img);
}

/// Every path-taking write entry point takes a non-UTF-8 path as bytes,
/// and each result is checked through `fs_ext4_lookup_at`, which shares no
/// path parsing with them.
#[test]
fn every_path_entry_point_takes_a_non_utf8_path_as_bytes() {
    let img = scratch();
    let fs = mount_rw(&img);
    let d = path(&[DIR]);
    let f = path(&[DIR, CAFE]);
    let payload = b"written by path\n";

    unsafe {
        // mkdir, create
        let dir_ino = fs_ext4_mkdir(fs, c(&d).as_ptr(), 0o755);
        assert_ne!(dir_ino, 0, "mkdir {:?}: {}", d, last_err());
        assert_eq!(lookup_at(fs, ROOT, DIR), Some(dir_ino));
        let file_ino = fs_ext4_create(fs, c(&f).as_ptr(), 0o644);
        assert_ne!(file_ino, 0, "create: {}", last_err());
        assert_eq!(lookup_at(fs, dir_ino, CAFE), Some(file_ino));

        // write_file, pwrite, truncate, fallocate
        let n = fs_ext4_write_file(
            fs,
            c(&f).as_ptr(),
            payload.as_ptr().cast(),
            payload.len() as u64,
        );
        assert_eq!(n, payload.len() as i64, "write_file: {}", last_err());
        let n = fs_ext4_pwrite(fs, c(&f).as_ptr(), b"W".as_ptr().cast(), 1, 0);
        assert_eq!(n, payload.len() as i64, "pwrite: {}", last_err());
        assert_eq!(read_all(fs, &f, payload.len()), b"Written by path\n");
        assert_eq!(fs_ext4_truncate(fs, c(&f).as_ptr(), 7), 0, "{}", last_err());
        assert_eq!(
            fs_ext4_fallocate(
                fs,
                c(&f).as_ptr(),
                1 << 20,
                4096,
                FS_EXT4_FALLOC_FL_KEEP_SIZE
            ),
            0,
            "fallocate: {}",
            last_err()
        );
        assert_eq!(read_all(fs, &f, 7), b"Written");

        // chmod, chown, utimens, set_flags
        assert_eq!(
            fs_ext4_chmod(fs, c(&f).as_ptr(), 0o600),
            0,
            "{}",
            last_err()
        );
        assert_eq!(
            fs_ext4_chown(fs, c(&f).as_ptr(), 1234, 5678),
            0,
            "{}",
            last_err()
        );
        assert_eq!(
            fs_ext4_utimens(fs, c(&f).as_ptr(), 1_000_000, 0, 2_000_000, 0),
            0,
            "utimens: {}",
            last_err()
        );
        let flags = stat(fs, &f).expect("stat before set_flags").inode_flags;
        assert_eq!(
            fs_ext4_set_flags(fs, c(&f).as_ptr(), flags | 0x40), // NODUMP
            0,
            "set_flags: {}",
            last_err()
        );
        let a = stat(fs, &f).expect("stat after the attribute calls");
        assert_eq!(a.inode, file_ino);
        assert_eq!(a.mode & 0o7777, 0o600);
        assert_eq!((a.uid, a.gid), (1234, 5678));
        assert_eq!(a.mtime, 2_000_000);
        assert_eq!(a.inode_flags & 0x40, 0x40, "NODUMP was not set");

        // setxattr, getxattr, listxattr, removexattr
        let name = c(b"user.k");
        assert_eq!(
            fs_ext4_setxattr(fs, c(&f).as_ptr(), name.as_ptr(), b"v".as_ptr().cast(), 1),
            0,
            "setxattr: {}",
            last_err()
        );
        let mut v = [0u8; 4];
        let n = fs_ext4_getxattr(
            fs,
            c(&f).as_ptr(),
            name.as_ptr(),
            v.as_mut_ptr().cast(),
            v.len(),
        );
        assert_eq!((n, v[0]), (1, b'v'), "getxattr: {}", last_err());
        let n = fs_ext4_listxattr(fs, c(&f).as_ptr(), std::ptr::null_mut(), 0);
        assert_eq!(n, 7, "listxattr: {}", last_err());
        assert_eq!(
            fs_ext4_removexattr(fs, c(&f).as_ptr(), name.as_ptr()),
            0,
            "removexattr: {}",
            last_err()
        );

        // link, symlink, readlink, mknod
        let l = path(&[b"l\xe9"]);
        assert_eq!(
            fs_ext4_link(fs, c(&f).as_ptr(), c(&l).as_ptr()),
            0,
            "{}",
            last_err()
        );
        assert_eq!(lookup_at(fs, ROOT, b"l\xe9"), Some(file_ino));
        let s = path(&[DIR, b"s\xe9"]);
        let target = b"../l\xe9";
        let s_ino = fs_ext4_symlink(fs, c(target).as_ptr(), c(&s).as_ptr());
        assert_ne!(s_ino, 0, "symlink: {}", last_err());
        assert_eq!(lookup_at(fs, dir_ino, b"s\xe9"), Some(s_ino));
        let mut buf = [0 as c_char; 32];
        let n = fs_ext4_readlink(fs, c(&s).as_ptr(), buf.as_mut_ptr(), buf.len());
        assert_eq!(n, target.len() as i32, "readlink: {}", last_err());
        let got: Vec<u8> = buf[..n as usize]
            .iter()
            .map(|&b| b.to_ne_bytes()[0])
            .collect();
        assert_eq!(got, target);
        let fifo = path(&[DIR, b"p\xfe"]);
        let p_ino = fs_ext4_mknod(fs, c(&fifo).as_ptr(), 0x1000 | 0o644, 0, 0);
        assert_ne!(p_ino, 0, "mknod: {}", last_err());
        assert_eq!(lookup_at(fs, dir_ino, b"p\xfe"), Some(p_ino));

        // rename, rename2, unlink, rmdir
        let moved = path(&[b"m\xe9"]);
        assert_eq!(
            fs_ext4_rename(fs, c(&f).as_ptr(), c(&moved).as_ptr()),
            0,
            "{}",
            last_err()
        );
        assert_eq!(lookup_at(fs, dir_ino, CAFE), None);
        assert_eq!(lookup_at(fs, ROOT, b"m\xe9"), Some(file_ino));
        // Onto a different file, which it replaces: renaming onto another
        // link of the same inode is a no-op under rename(2).
        let r = path(&[b"r\xe9"]);
        let r_ino = fs_ext4_create(fs, c(&r).as_ptr(), 0o644);
        assert_ne!(r_ino, 0, "create: {}", last_err());
        assert_eq!(
            fs_ext4_rename2(
                fs,
                c(&moved).as_ptr(),
                c(&r).as_ptr(),
                FS_EXT4_RENAME_REPLACE
            ),
            0,
            "rename2: {}",
            last_err()
        );
        assert_eq!(lookup_at(fs, ROOT, b"m\xe9"), None);
        assert_eq!(lookup_at(fs, ROOT, b"r\xe9"), Some(file_ino));
        assert_eq!(fs_ext4_unlink(fs, c(&r).as_ptr()), 0, "{}", last_err());
        assert_eq!(fs_ext4_unlink(fs, c(&l).as_ptr()), 0, "{}", last_err());
        assert_eq!(fs_ext4_unlink(fs, c(&s).as_ptr()), 0, "{}", last_err());
        assert_eq!(fs_ext4_unlink(fs, c(&fifo).as_ptr()), 0, "{}", last_err());
        assert_eq!(
            fs_ext4_rmdir(fs, c(&d).as_ptr()),
            0,
            "rmdir: {}",
            last_err()
        );
        assert_eq!(lookup_at(fs, ROOT, DIR), None);
        assert_eq!(lookup_at(fs, ROOT, b"l\xe9"), None);
        assert_eq!(lookup_at(fs, ROOT, b"r\xe9"), None);
    }

    unsafe { fs_ext4_umount(fs) };
    let _ = fs::remove_file(&img);
}
