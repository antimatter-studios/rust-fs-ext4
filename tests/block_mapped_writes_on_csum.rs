//! Writing and removing block-mapped (ext2/ext3-style) files keeps the
//! volume consistent, including on a volume with `metadata_csum` (#249).
//!
//! An ext3 volume tuned to `metadata_csum` keeps its block-mapped files, so
//! both properties meet on real volumes, and `mke2fs -O ^extent,^64bit,
//! metadata_csum` makes the same shape directly. Two defects showed on it:
//!
//! - `apply_replace_file_content` on a block-mapped file freed and marked
//!   blocks with the unbuffered helpers, which write the block bitmap
//!   without restamping its checksum: `Group 0 block bitmap does not match
//!   checksum`. The extent path went through the journaled buffer, which
//!   restamps.
//! - `apply_unlink` freed a file's blocks only when it had extents, so a
//!   block-mapped file's data and indirect blocks stayed allocated with
//!   nothing pointing at them. Renaming over one leaked the same way, and
//!   `apply_rmdir` or renaming over a block-mapped directory was refused
//!   as a corrupt extent tree.
//!
//! `e2fsck -fn` judges each step. The e2fsprogs tools run in the harness VM; a test fails when it cannot reach them.

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use std::sync::Arc;

fn mkfs(tag: &str, features: &str) -> String {
    let path =
        fs_ext4_test_support::temp_path!("fs_ext4_blockmap_{tag}_{}.img", std::process::id());
    std::fs::File::create(&path)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .unwrap();
    let out = fs_ext4_test_support::oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-O", features])
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

#[test]
fn block_mapped_files_write_and_unlink_cleanly() {
    // With and without checksums; the second is where the bitmap showed.
    for (tag, features) in [
        ("plain", "^extent,^64bit,^metadata_csum"),
        ("csum", "^extent,^64bit,metadata_csum"),
    ] {
        let path = mkfs(tag, features);
        let fs = mount(&path);
        fs.apply_create("/small", 0o644).unwrap();
        fs.apply_create("/big", 0o644).unwrap();
        drop(fs);
        e2fsck_clean(&path, &format!("{tag}: created"));

        // 20 KiB is direct blocks only; 64 MiB of 4 KiB blocks would be
        // too big, so 13 blocks forces the single-indirect tier.
        let fs = mount(&path);
        fs.apply_replace_file_content("/small", &[7u8; 20_000])
            .unwrap();
        fs.apply_replace_file_content("/big", &vec![9u8; 13 * 4096])
            .unwrap();
        drop(fs);
        e2fsck_clean(&path, &format!("{tag}: written"));

        let fs = mount(&path);
        fs.apply_replace_file_content("/small", &[8u8; 9000])
            .unwrap();
        drop(fs);
        e2fsck_clean(&path, &format!("{tag}: rewritten"));

        let fs = mount(&path);
        fs.apply_unlink("/small").unwrap();
        fs.apply_unlink("/big").unwrap();
        drop(fs);
        e2fsck_clean(&path, &format!("{tag}: unlinked"));

        // Directories are block-mapped too, and a rename can reap either.
        let fs = mount(&path);
        fs.apply_mkdir("/d", 0o755).unwrap();
        fs.apply_mkdir("/e", 0o755).unwrap();
        fs.apply_mkdir("/gone", 0o755).unwrap();
        fs.apply_create("/f", 0o644).unwrap();
        fs.apply_create("/g", 0o644).unwrap();
        fs.apply_replace_file_content("/g", &[1u8; 30_000]).unwrap();
        drop(fs);
        e2fsck_clean(&path, &format!("{tag}: populated"));

        let fs = mount(&path);
        fs.apply_rmdir("/gone")
            .expect("rmdir a block-mapped directory");
        fs.apply_rename("/f", "/g", true)
            .expect("rename over a block-mapped file");
        fs.apply_rename("/d", "/e", true)
            .expect("rename over a block-mapped directory");
        drop(fs);
        e2fsck_clean(&path, &format!("{tag}: reaped by rmdir and rename"));
        let _ = std::fs::remove_file(&path);
    }
}
