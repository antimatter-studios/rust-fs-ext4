//! Every block that leaves a file's extent tree is freed.
//!
//! Two ways of misreading which blocks a file holds left blocks marked in
//! use with nothing pointing at them:
//!
//! - **Size taken for allocation.** Unlink, replace-content and
//!   rename-over freed a file's blocks only when `i_size > 0`, and freed
//!   them through a shrink plan. A file of size zero can hold blocks: a
//!   `KEEP_SIZE` preallocation is exactly that. Its extent tree was
//!   dropped and its blocks kept.
//! - **Leaves taken for the tree.** A punch-hole flattened the tree to its
//!   leaf extents and wrote what survived back into the inode's inline
//!   root. On a tree deeper than the root, the index and leaf blocks
//!   that had held those extents were neither referenced nor freed. And
//!   an unlink of such a file was refused, because the shrink plan only
//!   reads an inline root.
//!
//! Volumes come from `mkfs.ext4`, and `e2fsck -fn` judges each result.
//! The e2fsprogs tools run in the harness VM; a test fails when it cannot reach them.

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use std::sync::Arc;

fn mkfs(tag: &str) -> String {
    let mkfs = "mkfs.ext4";
    let path = fs_ext4_test_support::temp_path!("fs_ext4_leak_{tag}_{}.img", std::process::id());
    std::fs::File::create(&path)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .unwrap();
    let out = fs_ext4_test_support::oracle(mkfs)
        .args(["-q", "-F", "-b", "4096"])
        .arg(&path)
        .output();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

fn e2fsck_clean(path: &str, what: &str) {
    fs_ext4_test_support::assert_e2fsck_clean(path, what);
}

fn mount(path: &str) -> Filesystem {
    Filesystem::mount(Arc::new(FileDevice::open_rw(path).unwrap())).unwrap()
}

/// A volume holding `/f`: empty, with 64 KiB preallocated past its end.
fn preallocated(tag: &str) -> (String, u32) {
    let path = mkfs(tag);
    let fs = mount(&path);
    let ino = fs.apply_create("/f", 0o644).unwrap();
    fs.apply_fallocate_keep_size(ino, 0, 64 * 1024).unwrap();
    drop(fs);
    e2fsck_clean(&path, tag);
    (path, ino)
}

/// A volume holding `/f` with twelve one-block extents, one every other
/// block, so its extent tree is deeper than the inode's inline root.
fn deep(tag: &str) -> (String, u32) {
    let path = mkfs(tag);
    let fs = mount(&path);
    let ino = fs.apply_create("/f", 0o644).unwrap();
    for i in 0..12u64 {
        fs.apply_pwrite("/f", i * 2 * 4096, &[0x5a; 4096]).unwrap();
    }
    let (inode, _) = fs.read_inode_verified(ino).unwrap();
    assert!(
        u16::from_le_bytes([inode.block[6], inode.block[7]]) >= 1,
        "[{tag}] the tree is still inline"
    );
    drop(fs);
    e2fsck_clean(&path, tag);
    (path, ino)
}

#[test]
fn unlinking_an_empty_file_frees_its_preallocation() {
    let (path, _) = preallocated("unlink");
    mount(&path).apply_unlink("/f").expect("unlink");
    e2fsck_clean(&path, "unlink");
}

#[test]
fn replacing_an_empty_files_content_frees_its_preallocation() {
    let (path, _) = preallocated("replace");
    mount(&path)
        .apply_replace_file_content("/f", b"hello")
        .expect("replace");
    e2fsck_clean(&path, "replace");
}

#[test]
fn renaming_over_an_empty_file_frees_its_preallocation() {
    let (path, _) = preallocated("rename");
    let fs = mount(&path);
    fs.apply_create("/g", 0o644).unwrap();
    fs.apply_rename("/g", "/f", true).expect("rename over");
    drop(fs);
    e2fsck_clean(&path, "rename");
}

#[test]
fn unlinking_a_file_with_a_deep_extent_tree_frees_the_tree() {
    let (path, _) = deep("unlink_deep");
    mount(&path).apply_unlink("/f").expect("unlink");
    e2fsck_clean(&path, "unlink_deep");
}

#[test]
fn punching_a_deep_tree_down_to_its_root_frees_the_tree() {
    let (path, ino) = deep("punch");
    mount(&path)
        .apply_fallocate_punch_hole(ino, 0, 16 * 4096)
        .expect("punch");
    e2fsck_clean(&path, "punch");
}
