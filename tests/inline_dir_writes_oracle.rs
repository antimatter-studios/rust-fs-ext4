//! Mutations of inline-data directories, judged by e2fsprogs (#382, #428).
//!
//! An inline directory's `i_block` holds its parent's inode number and then
//! entries. Every directory writer once read it as a block map, so renaming
//! one across parents wrote the new parent's number into the block its old
//! parent's inode number named (#382); those writes were then refused. They
//! now go through: an entry is added and removed in the inode while it
//! fits, and the directory is converted to a block one when it does not.
//! The directories here are made by `debugfs mkdir` on a
//! `mkfs.ext4 -O inline_data` volume — the independent layout, not one this
//! crate patched in — and `e2fsck -fn` judges what is left, and `debugfs ls`
//! lists it. The e2fsprogs tools run in the harness VM; a test fails when it
//! cannot reach them.

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use std::sync::Arc;

fn mount(path: &str) -> Filesystem {
    Filesystem::mount(Arc::new(FileDevice::open_rw(path).unwrap())).unwrap()
}

fn inode_of(fs: &Filesystem, path: &str) -> fs_ext4::inode::Inode {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    let ino = fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, path).unwrap();
    fs.read_inode_verified(ino).unwrap().0
}

/// The names `debugfs ls` lists in `dir`, `.` and `..` excluded, sorted.
#[track_caller]
fn debugfs_ls(image: &str, dir: &str) -> Vec<String> {
    let out = fs_ext4_test_support::oracle("debugfs")
        .args(["-R", &format!("ls -p {dir}"), image])
        .output();
    assert!(out.status.success(), "debugfs ls {dir}");
    // `ls -p`: /inode/mode/uid/gid/name/size/ per entry.
    let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split('/').nth(5).map(str::to_string))
        .filter(|n| !n.is_empty() && n != "." && n != "..")
        .collect();
    names.sort();
    names
}

#[test]
fn mutations_of_inline_directories_made_by_debugfs_leave_e2fsck_clean() {
    let image = fs_ext4_test_support::temp_path!("fs_ext4_inline_dirs_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(32 * 1024 * 1024))
        .unwrap();
    let made = fs_ext4_test_support::oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", "256"])
        .args(["-O", "inline_data,metadata_csum,^has_journal"])
        .arg(&image)
        .output();
    assert!(
        made.status.success(),
        "mkfs.ext4: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    fs_ext4_test_support::oracle("debugfs")
        .args(["-w", "-f", "-"])
        .arg(&image)
        .stdin("mkdir /a\nmkdir /b\n")
        .judged()
        .clean("debugfs mkdir");

    let fs = mount(&image);
    for dir in ["/a", "/b"] {
        assert!(
            inode_of(&fs, dir).has_inline_data(),
            "{dir}: debugfs made a block directory, so this test proves nothing"
        );
    }
    // A block directory of our own to move things in and out of.
    fs.apply_mkdir("/c", 0o755).unwrap();
    fs.apply_create("/c/x", 0o644).unwrap();
    fs.apply_create("/a/g", 0o644).unwrap();
    fs.apply_mkdir("/a/h", 0o755).unwrap();
    fs.apply_symlink("target", "/a/s").unwrap();
    fs.apply_link("/c/x", "/a/lx").unwrap();
    fs.apply_unlink("/a/g").unwrap();
    fs.apply_rename("/a/s", "/c/s", false).unwrap();
    fs.apply_rename("/c/x", "/a/x2", false).unwrap();
    fs.apply_rename("/a/h", "/a/h2", false).unwrap();
    fs.apply_rename("/b", "/c/b", false).unwrap();
    fs.apply_rmdir("/c/b").unwrap();
    assert!(inode_of(&fs, "/a").has_inline_data(), "/a stayed inline");
    drop(fs);
    fs_ext4_test_support::assert_e2fsck_clean(&image, "inline dirs: edited in the inode");
    assert_eq!(debugfs_ls(&image, "/a"), ["h2", "lx", "x2"]);
    assert_eq!(debugfs_ls(&image, "/c"), ["s"]);

    // Enough entries that /a outgrows its inode and is converted.
    let fs = mount(&image);
    for i in 0..16 {
        fs.apply_create(&format!("/a/entry-{i:02}"), 0o644).unwrap();
    }
    assert!(!inode_of(&fs, "/a").has_inline_data(), "/a was converted");
    drop(fs);
    fs_ext4_test_support::assert_e2fsck_clean(&image, "inline dirs: converted");
    let mut want: Vec<String> = (0..16).map(|i| format!("entry-{i:02}")).collect();
    want.extend(["h2", "lx", "x2"].map(String::from));
    want.sort();
    assert_eq!(debugfs_ls(&image, "/a"), want);
    let _ = std::fs::remove_file(&image);
}
