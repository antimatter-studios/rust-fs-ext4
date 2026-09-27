//! Symlink targets as `debugfs` reads them, against `fs_ext4_readlink`.
//! Runs e2fsprogs in the harness VM; fails when it cannot. #290.
//!
//! The expected targets are never our own reading of the inode:
//!
//!   * ext4-basic.img's /link.txt was made by the kernel (`ln -s test.txt`);
//!     `debugfs -R "stat"` reports its inline target as `Fast link dest`.
//!   * A volume formatted by `mkfs.ext4` gets symlinks made by `debugfs
//!     symlink` — libext2fs decides fast (target < 60 bytes, inline in
//!     i_block) or slow (a data block) — at 1, 59, 60, 61 and 200 bytes,
//!     either side of the 60-byte i_block boundary. `debugfs` then reports
//!     each one's size and, for the fast ones, the inline target.

use fs_ext4::capi::*;
use fs_ext4_test_support::oracle;
use std::ffi::{CStr, CString};

fn mount(image: &str) -> *mut fs_ext4_fs_t {
    let p = CString::new(image).unwrap();
    let fs = unsafe { fs_ext4_mount(p.as_ptr()) };
    assert!(
        !fs.is_null(),
        "fs_ext4_mount({image}): {}",
        unsafe { CStr::from_ptr(fs_ext4_last_error()) }.to_string_lossy()
    );
    fs
}

fn readlink(fs: *mut fs_ext4_fs_t, path: &str) -> Vec<u8> {
    let p = CString::new(path).unwrap();
    let mut buf = vec![0xAAu8; 4096];
    let rc = unsafe { fs_ext4_readlink(fs, p.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    assert!(
        rc >= 0,
        "readlink {path}: {}",
        unsafe { CStr::from_ptr(fs_ext4_last_error()) }.to_string_lossy()
    );
    let n = rc as usize;
    assert_eq!(
        buf[n], 0,
        "readlink {path}: no NUL after the {n} bytes returned"
    );
    buf.truncate(n);
    buf
}

/// `debugfs -R "stat <path>"`'s report on `image`.
fn debugfs_stat(image: &str, path: &str) -> String {
    let out = oracle("debugfs")
        .arg("-R")
        .arg(format!("stat {path}"))
        .arg(image)
        .judged()
        .clean(&format!("debugfs stat {path}"));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The inline target `debugfs` reports for a fast symlink, or None.
fn fast_link_dest(stat: &str) -> Option<String> {
    let line = stat.lines().find(|l| l.contains("Fast link dest:"))?;
    let quoted = line.split_once("Fast link dest: \"")?.1;
    Some(quoted.strip_suffix('"')?.to_string())
}

/// `Size: N` from a `debugfs stat` report.
fn size(stat: &str) -> u64 {
    stat.split_whitespace()
        .skip_while(|w| *w != "Size:")
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no Size: in debugfs stat:\n{stat}"))
}

#[test]
fn basic_fixture_fast_symlink_matches_debugfs() {
    let image = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), "ext4-basic.img");
    let stat = debugfs_stat(&image, "/link.txt");
    let expected = fast_link_dest(&stat)
        .unwrap_or_else(|| panic!("debugfs does not call /link.txt a fast symlink:\n{stat}"));
    assert_eq!(expected, "test.txt", "the fixture's link changed");

    let fs = mount(&image);
    assert_eq!(readlink(fs, "/link.txt"), expected.as_bytes());
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn symlinks_either_side_of_the_i_block_boundary_match_debugfs() {
    let image =
        fs_ext4_test_support::temp_path!("fs_ext4_readlink_oracle_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(16 * 1024 * 1024))
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

    let lengths = [1usize, 59, 60, 61, 200];
    let targets: Vec<(String, String)> = lengths
        .iter()
        .map(|&n| {
            // Distinct per length, and not one repeated byte, so a
            // shifted or truncated copy cannot pass.
            let target: String = (0..n).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
            (format!("/len{n}"), target)
        })
        .collect();
    let script: String = targets
        .iter()
        .map(|(link, target)| format!("symlink {link} {target}\n"))
        .collect();
    oracle("debugfs")
        .args(["-w", "-f", "-"])
        .arg(&image)
        .stdin(script)
        .judged()
        .clean("debugfs symlink");

    let fs = mount(&image);
    for (link, target) in &targets {
        let stat = debugfs_stat(&image, link);
        assert_eq!(size(&stat), target.len() as u64, "{link}: debugfs size");
        let fast = fast_link_dest(&stat);
        if target.len() < 60 {
            assert_eq!(
                fast.as_deref(),
                Some(target.as_str()),
                "{link}: debugfs fast target"
            );
        } else {
            assert_eq!(
                fast,
                None,
                "{link}: debugfs calls a {}-byte target fast",
                target.len()
            );
        }
        assert_eq!(
            String::from_utf8_lossy(&readlink(fs, link)),
            *target,
            "{link}: fs_ext4_readlink"
        );
    }
    unsafe { fs_ext4_umount(fs) };
    std::fs::remove_file(&image).ok();
}
