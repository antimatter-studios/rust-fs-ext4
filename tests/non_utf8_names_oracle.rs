//! A name `debugfs` filed as non-UTF-8 bytes is reachable, by path, by
//! those bytes — and what this driver files by such a path, `debugfs` finds
//! by the same bytes and `e2fsck` passes. Runs e2fsprogs in the harness VM;
//! fails when it cannot. #418.
//!
//! ext4 directory entry names are raw bytes with no encoding rule. The C
//! ABI decoded its paths as UTF-8 and answered `""` — the root — for one
//! that did not decode, so a name like these was unreachable and a caller
//! handing back a name `fs_ext4_dir_next` had just listed got the root.
//!
//! The names are not this driver's: libext2fs files them, through
//! `debugfs`, into a volume `mkfs.ext4` formatted. A driver that resolved
//! paths its own way would agree with its own fixtures and still miss
//! these. In the other direction the checks are `debugfs cat`, which walks
//! the path with libext2fs's own byte comparison, and `e2fsck -fn`.

use fs_ext4::capi::*;
use fs_ext4_test_support::{assert_e2fsck_clean, oracle, temp_path};
use std::ffi::{CStr, OsStr};
use std::os::raw::{c_char, c_void};
use std::os::unix::ffi::OsStrExt;

/// `café.txt` as a latin-1 machine writes it: `\xe9` alone is not UTF-8.
const CAFE: &[u8] = b"caf\xe9.txt";
const DIR: &[u8] = b"d\xff";
const LINK: &[u8] = b"s\xe9";
const TARGET: &[u8] = b"t\xfe\xff";
const MOVED_TO: &[u8] = b"n\xe9";

fn last_err() -> String {
    unsafe {
        CStr::from_ptr(fs_ext4_last_error())
            .to_string_lossy()
            .into_owned()
    }
}

/// `bytes` as a NUL-terminated C string, on signed and unsigned `c_char`.
fn c(bytes: &[u8]) -> Vec<c_char> {
    bytes
        .iter()
        .chain(std::iter::once(&0))
        .map(|&b| c_char::from_ne_bytes([b]))
        .collect()
}

fn path(components: &[&[u8]]) -> Vec<u8> {
    components
        .iter()
        .flat_map(|c| [b"/", *c].concat())
        .collect()
}

/// A volume formatted by `mkfs.ext4` and populated by `debugfs` with a
/// directory, a file and a symlink whose names are not UTF-8.
fn volume() -> String {
    let image = temp_path!("fs_ext4_non_utf8_names_{}.img", std::process::id());
    let _ = std::fs::remove_file(&image);
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .unwrap();
    let out = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096"])
        .arg(&image)
        .output();
    assert!(
        out.status.success(),
        "mkfs.ext4: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // `write` and `symlink` link their name into the current directory
    // as given, slashes and all, so they run after a `cd`.
    let script = [
        &b"mkdir "[..],
        DIR,
        b"\ncd ",
        DIR,
        b"\nwrite /dev/null ",
        CAFE,
        b"\nsymlink ",
        LINK,
        b" ",
        TARGET,
        b"\n",
    ]
    .concat();
    oracle("debugfs")
        .args(["-w", "-f", "-"])
        .arg(&image)
        .stdin(script)
        .output();
    assert_e2fsck_clean(&image, "the volume debugfs populated");
    image
}

/// Every (name, inode) `fs_ext4_dir_open` lists for path `p`.
fn list(fs: *mut fs_ext4_fs_t, p: &[u8]) -> Vec<(Vec<u8>, u32)> {
    let iter = unsafe { fs_ext4_dir_open(fs, c(p).as_ptr()) };
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
        let name = d.name[..d.name_len as usize]
            .iter()
            .map(|&b| b.to_ne_bytes()[0])
            .collect();
        out.push((name, d.inode));
    }
    unsafe { fs_ext4_dir_close(iter) };
    out
}

fn listed(entries: &[(Vec<u8>, u32)], name: &[u8]) -> (Vec<u8>, u32) {
    entries
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| {
            panic!(
                "{:?} is not listed byte for byte",
                String::from_utf8_lossy(name)
            )
        })
        .clone()
}

fn stat_ino(fs: *mut fs_ext4_fs_t, p: &[u8]) -> u32 {
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_ext4_stat(fs, c(p).as_ptr(), &mut attr) };
    assert_eq!(
        rc,
        0,
        "stat {:?}: {}",
        String::from_utf8_lossy(p),
        last_err()
    );
    attr.inode
}

#[test]
fn a_name_debugfs_filed_as_non_utf8_bytes_is_reachable_by_those_bytes() {
    let image = volume();
    let payload = b"written through a path that is not UTF-8\n";

    unsafe {
        let fs = fs_ext4_mount_rw(c(image.as_bytes()).as_ptr());
        assert!(!fs.is_null(), "mount_rw: {}", last_err());

        // The host's walk: list, and hand each listed name back as a path.
        let (dir_name, dir_ino) = listed(&list(fs, b"/"), DIR);
        assert_eq!(stat_ino(fs, &path(&[&dir_name])), dir_ino);
        let inside = list(fs, &path(&[&dir_name]));
        let (file_name, file_ino) = listed(&inside, CAFE);
        let (link_name, link_ino) = listed(&inside, LINK);
        let file = path(&[&dir_name, &file_name]);
        let link = path(&[&dir_name, &link_name]);
        assert_eq!(stat_ino(fs, &file), file_ino);
        assert_eq!(stat_ino(fs, &link), link_ino);

        let mut buf = [0 as c_char; 32];
        let n = fs_ext4_readlink(fs, c(&link).as_ptr(), buf.as_mut_ptr(), buf.len());
        assert_eq!(n, TARGET.len() as i32, "readlink: {}", last_err());
        let target: Vec<u8> = buf[..n as usize]
            .iter()
            .map(|&b| b.to_ne_bytes()[0])
            .collect();
        assert_eq!(target, TARGET, "the target debugfs wrote");

        // And write by those paths, for debugfs and e2fsck to judge.
        let n = fs_ext4_write_file(
            fs,
            c(&file).as_ptr(),
            payload.as_ptr().cast::<c_void>(),
            payload.len() as u64,
        );
        assert_eq!(n, payload.len() as i64, "write_file: {}", last_err());
        let new_dir = path(&[MOVED_TO]);
        assert_ne!(
            fs_ext4_mkdir(fs, c(&new_dir).as_ptr(), 0o755),
            0,
            "mkdir: {}",
            last_err()
        );
        let moved = path(&[MOVED_TO, CAFE]);
        assert_eq!(
            fs_ext4_rename(fs, c(&file).as_ptr(), c(&moved).as_ptr()),
            0,
            "rename: {}",
            last_err()
        );
        assert_eq!(fs_ext4_unlink(fs, c(&link).as_ptr()), 0, "{}", last_err());
        fs_ext4_umount(fs);
    }

    // libext2fs walks the new path with its own byte comparison.
    let request = [&b"cat "[..], &path(&[MOVED_TO, CAFE])].concat();
    let out = oracle("debugfs")
        .arg("-R")
        .arg(OsStr::from_bytes(&request))
        .arg(&image)
        .output();
    assert_eq!(
        out.stdout, payload,
        "debugfs read other bytes at the path this driver filed"
    );
    // The old entry and the symlink are gone with their counts right, and
    // nothing else changed shape.
    assert_e2fsck_clean(&image, "after writes through non-UTF-8 paths");
    let _ = std::fs::remove_file(&image);
}
