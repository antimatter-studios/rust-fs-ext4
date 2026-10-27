//! An attribute stored in the inode sets `COMPAT_EXT_ATTR`.
//!
//! A volume without the feature carries no extended attributes as far as
//! e2fsprogs is concerned: `debugfs ea_list` lists nothing and `e2fsck`
//! ignores the area. The external-block path sets the feature with the
//! first block it writes (#88), but an attribute that fits in the inode
//! never reached that path, so a volume whose attributes are all small --
//! security labels, typically -- kept them where no tool would read them.
//!
//! Each volume starts without the feature: one made by `mkfs.ext4 -O
//! ^ext_attr`, one by this crate's mkfs, which does not set it. The driver
//! sets one small attribute; `debugfs ea_list` must list it and `e2fsck -fn`
//! must accept the volume. Both run in the harness VM.

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use std::sync::Arc;

const SIZE: u64 = 64 * 1024 * 1024;

fn image(tag: &str) -> String {
    let path = fs_ext4_test_support::temp_path!(
        "fs_ext4_xattr_in_inode_ext_attr_{tag}_{}.img",
        std::process::id()
    );
    std::fs::File::create(&path)
        .and_then(|f| f.set_len(SIZE))
        .unwrap();
    path
}

fn mkfs_without_ext_attr() -> String {
    let path = image("mke2fs");
    let out = fs_ext4_test_support::oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", "256", "-O", "^ext_attr"])
        .arg(&path)
        .output();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

fn mkfs_by_this_crate() -> String {
    let path = image("crate");
    let dev = FileDevice::open_rw(&path).unwrap();
    fs_ext4::mkfs::format_filesystem(&dev, Some("xattr"), None, SIZE, 4096).unwrap();
    path
}

/// Every line of `debugfs -R 'ea_list <file>'` that lists `name`.
fn listed(image: &str, file: &str, name: &str) -> Vec<String> {
    let out = fs_ext4_test_support::oracle("debugfs")
        .arg("-R")
        .arg(format!("ea_list {file}"))
        .arg(image)
        .output();
    assert!(
        out.status.success(),
        "debugfs ea_list {file}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let prefix = format!("{name} (");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with(&prefix))
        .map(str::to_string)
        .collect()
}

fn small_attribute_is_listed(image: &str, what: &str) {
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(image).unwrap())).unwrap();
        fs.apply_create("/f", 0o644).unwrap();
        // Eight bytes fit the in-inode area of a 256-byte inode.
        fs.apply_setxattr("/f", "user.a", b"in-inode").unwrap();
    }
    let found = listed(image, "/f", "user.a");
    assert!(
        found.len() == 1 && found[0].starts_with("user.a (8)"),
        "{what}: debugfs lists {found:?}; want the 8-byte attribute"
    );
    fs_ext4_test_support::assert_e2fsck_clean(image, what);
    let _ = std::fs::remove_file(image);
}

#[test]
fn an_in_inode_attribute_on_a_mke2fs_volume_without_ext_attr_is_listed() {
    small_attribute_is_listed(&mkfs_without_ext_attr(), "mkfs.ext4 -O ^ext_attr");
}

#[test]
fn an_in_inode_attribute_on_a_volume_this_crate_formatted_is_listed() {
    small_attribute_is_listed(&mkfs_by_this_crate(), "this crate's mkfs");
}
