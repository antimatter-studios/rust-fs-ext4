//! The C ABI's attribute calls take every name the kernel can set, not
//! only the UTF-8 ones (#591).
//!
//! Linux takes any bytes but NUL in an attribute name. The guest's
//! `setfattr` sets three on one file, two of them not UTF-8, and its own
//! `getfattr -d -m -` says what it set. Then, through `fs_ext4_*`:
//!
//! - `fs_ext4_listxattr` lists exactly those names, as bytes;
//! - `fs_ext4_getxattr` given each one returns the guest's value;
//! - `fs_ext4_setxattr` replaces the value of one such name and adds
//!   another, and `fs_ext4_removexattr` removes a third.
//!
//! The kernel then mounts the volume again, and its `getfattr` must list
//! the names and values those three calls left.

use fs_ext4::capi::*;
use fs_ext4_test_support::{assert_e2fsck_clean, guest_kernel_write, oracle, temp_path};
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};

const FILL: &str = r#"cd "$MNT"
printf 'raw names\n' > f
setfattr -n user.plain -v 0x706c61696e f
setfattr -n "$(printf 'user.raw-\377')" -v 0x7261770a f
setfattr -n "$(printf 'trusted.raw-\376')" -v 0x01fe f
sync
"#;

/// Every attribute of `/f`, one `D|NAME=VALUE` line each, as the guest's
/// `getfattr -d -m - -e hex` prints them.
const DUMP: &str = r#"getfattr -h -d -m - -e hex --absolute-names "$MNT/f" | awk 'NF && !/^#/ { print "D|" $0 }'
"#;

const PLAIN: &[u8] = b"user.plain";
const RAW_USER: &[u8] = b"user.raw-\xff";
const RAW_TRUSTED: &[u8] = b"trusted.raw-\xfe";
/// The name `fs_ext4_setxattr` adds, also not UTF-8.
const RAW_NEW: &[u8] = b"user.new-\xfd";

fn volume() -> String {
    let image = temp_path!("fs_ext4_capi_xattr_raw_{}.img", std::process::id());
    let _ = std::fs::remove_file(&image);
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(32 * 1024 * 1024))
        .unwrap_or_else(|e| panic!("size {image}: {e}"));
    let out = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", "256"])
        .arg(&image)
        .output();
    assert!(
        out.status.success(),
        "mkfs.ext4 {image}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    image
}

/// A name as getfattr prints it, its bytes back: `\` and three octal
/// digits is one byte, and a byte above 0x7f is printed as itself.
fn unquote(name: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len());
    let mut i = 0;
    while i < name.len() {
        let octal = name
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match (name[i], octal) {
            (b'\\', Some(d)) => {
                out.push(d.iter().fold(0u8, |n, c| n.wrapping_mul(8) + (c - b'0')));
                i += 4;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// What the guest's `getfattr` printed, read as BYTES: a lossy read as
/// text would turn a byte above 0x7f into U+FFFD, a name the volume does
/// not hold.
fn attributes(stdout: &[u8]) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut out = BTreeMap::new();
    for line in stdout.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let shown = String::from_utf8_lossy(line);
        let line = line
            .strip_prefix(b"D|")
            .unwrap_or_else(|| panic!("a report line: {shown}"));
        let (name, value) = match line.iter().position(|&b| b == b'=') {
            Some(at) => (&line[..at], &line[at + 1..]),
            None => (line, &b""[..]),
        };
        let value = match value {
            b"" | b"\"\"" => Vec::new(),
            hex => hex::decode(
                hex.strip_prefix(b"0x")
                    .unwrap_or_else(|| panic!("a getfattr -e hex value: {shown}")),
            )
            .unwrap_or_else(|e| panic!("{shown}: {e}")),
        };
        assert!(out.insert(unquote(name), value).is_none(), "twice: {shown}");
    }
    out
}

/// Run `script` against the kernel's mount of `image`, then dump `/f`.
fn kernel_dump(image: &str, script: &str) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let out = guest_kernel_write(image, &format!("{script}{DUMP}"));
    assert!(
        out.status.success(),
        "the guest failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    attributes(&out.stdout)
}

fn last_error() -> String {
    unsafe {
        CStr::from_ptr(fs_ext4_last_error())
            .to_string_lossy()
            .into_owned()
    }
}

fn shown(name: &[u8]) -> String {
    String::from_utf8_lossy(name).into_owned()
}

/// `fs_ext4_listxattr` on `path`, split into its names.
unsafe fn list(fs: *mut fs_ext4_fs_t, path: &CStr) -> Vec<Vec<u8>> {
    let need = fs_ext4_listxattr(fs, path.as_ptr(), std::ptr::null_mut(), 0);
    assert!(need >= 0, "listxattr probe: {}", last_error());
    let mut buf = vec![0u8; need as usize];
    let got = fs_ext4_listxattr(
        fs,
        path.as_ptr(),
        buf.as_mut_ptr().cast::<c_char>(),
        buf.len(),
    );
    assert_eq!(got, need, "listxattr: {}", last_error());
    let mut names: Vec<Vec<u8>> = buf
        .split(|&b| b == 0)
        .filter(|n| !n.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    names.sort();
    names
}

/// `fs_ext4_getxattr` on `path` for `name`.
unsafe fn get(fs: *mut fs_ext4_fs_t, path: &CStr, name: &[u8]) -> Vec<u8> {
    let name_c = CString::new(name).unwrap();
    let need = fs_ext4_getxattr(fs, path.as_ptr(), name_c.as_ptr(), std::ptr::null_mut(), 0);
    assert!(
        need >= 0,
        "getxattr {}: {} (errno {})",
        shown(name),
        last_error(),
        fs_ext4_last_errno()
    );
    let mut buf = vec![0u8; need as usize];
    let got = fs_ext4_getxattr(
        fs,
        path.as_ptr(),
        name_c.as_ptr(),
        buf.as_mut_ptr().cast::<c_void>(),
        buf.len(),
    );
    assert_eq!(got, need, "getxattr {}: {}", shown(name), last_error());
    buf
}

#[test]
fn the_c_abi_reads_writes_and_removes_names_the_kernel_set_that_are_not_utf8() {
    let image = volume();
    let set = kernel_dump(&image, FILL);
    let expected: BTreeMap<Vec<u8>, Vec<u8>> = [
        (PLAIN.to_vec(), b"plain".to_vec()),
        (RAW_USER.to_vec(), b"raw\n".to_vec()),
        (RAW_TRUSTED.to_vec(), b"\x01\xfe".to_vec()),
    ]
    .into();
    assert_eq!(set, expected, "the guest's getfattr after setfattr");

    let image_c = CString::new(image.as_str()).unwrap();
    let path = c"/f";
    unsafe {
        let fs = fs_ext4_mount_rw(image_c.as_ptr());
        assert!(!fs.is_null(), "mount rw: {}", last_error());

        assert_eq!(
            list(fs, path),
            set.keys().cloned().collect::<Vec<_>>(),
            "fs_ext4_listxattr against the guest's getfattr -m -"
        );
        for (name, value) in &set {
            assert_eq!(&get(fs, path, name), value, "{}", shown(name));
        }

        let replaced = b"replaced";
        let added = b"\x00\xfd";
        for (name, value) in [(RAW_USER, &replaced[..]), (RAW_NEW, &added[..])] {
            let name_c = CString::new(name).unwrap();
            assert_eq!(
                fs_ext4_setxattr(
                    fs,
                    path.as_ptr(),
                    name_c.as_ptr(),
                    value.as_ptr().cast::<c_void>(),
                    value.len(),
                ),
                0,
                "setxattr {}: {} (errno {})",
                shown(name),
                last_error(),
                fs_ext4_last_errno()
            );
        }
        let trusted_c = CString::new(RAW_TRUSTED).unwrap();
        assert_eq!(
            fs_ext4_removexattr(fs, path.as_ptr(), trusted_c.as_ptr()),
            0,
            "removexattr {}: {} (errno {})",
            shown(RAW_TRUSTED),
            last_error(),
            fs_ext4_last_errno()
        );
        fs_ext4_umount(fs);
    }

    assert_e2fsck_clean(&image, "capi_xattr_raw_names");
    let after = kernel_dump(&image, "");
    let expected: BTreeMap<Vec<u8>, Vec<u8>> = [
        (PLAIN.to_vec(), b"plain".to_vec()),
        (RAW_USER.to_vec(), b"replaced".to_vec()),
        (RAW_NEW.to_vec(), b"\x00\xfd".to_vec()),
    ]
    .into();
    assert_eq!(
        after, expected,
        "the guest's getfattr after fs_ext4_setxattr and fs_ext4_removexattr"
    );
}
