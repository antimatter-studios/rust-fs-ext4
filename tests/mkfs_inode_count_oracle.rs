//! A requested inode count, held to `mke2fs -N` in the guest.
//!
//! `tests/mkfs_inode_count.rs` states the rounding this crate applies;
//! this proves it is the rounding `mke2fs` applies. For each case both
//! format the same size, block size and inode size with the same request,
//! and `dumpe2fs -h` must report the same `Inode count` and `Inodes per
//! group` for both. The volume this crate made must pass `e2fsck -fn`.

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::features::FsFlavor;
use fs_ext4::mkfs::{self, FormatOptions};
use fs_ext4_test_support::{assert_e2fsck_clean, oracle, temp_path};

fn scratch(tag: &str, who: &str, size: u64) -> String {
    let path = temp_path!("fs_ext4_inodes_{who}_{tag}_{}.img", std::process::id());
    std::fs::File::create(&path)
        .and_then(|f| f.set_len(size))
        .unwrap_or_else(|e| panic!("create {path}: {e}"));
    path
}

/// `Inode count` and `Inodes per group` from `dumpe2fs -h`.
fn inode_geometry(image: &str) -> (String, String) {
    let out = oracle("dumpe2fs")
        .args(["-h", image])
        .judged()
        .clean("dumpe2fs");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .map(|v| v.trim().to_owned())
            .unwrap_or_else(|| panic!("dumpe2fs -h {image}: no {name:?} in\n{text}"))
    };
    (field("Inode count"), field("Inodes per group"))
}

fn matches_mke2fs(tag: &str, flavor: FsFlavor, block_size: u32, size: u64, requested: u32) {
    let (kind, inode_size) = match flavor {
        FsFlavor::Ext2 => ("ext2", "128"),
        FsFlavor::Ext3 => ("ext3", "128"),
        FsFlavor::Ext4 => ("ext4", "256"),
    };

    let ours = scratch(tag, "ours", size);
    {
        let dev = FileDevice::open_rw(&ours).expect("open_rw");
        let opts = FormatOptions {
            flavor,
            inodes_count: Some(requested),
        };
        mkfs::format_filesystem_with_options(&dev, None, None, size, block_size, &opts)
            .unwrap_or_else(|e| panic!("[{tag}] format: {e:?}"));
        dev.flush().expect("flush");
    }
    assert_e2fsck_clean(&ours, tag);

    let theirs = scratch(tag, "mke2fs", size);
    let made = oracle("mke2fs")
        .args(["-q", "-F", "-t", kind, "-I", inode_size, "-b"])
        .arg(block_size.to_string())
        .arg("-N")
        .arg(requested.to_string())
        .arg(&theirs)
        .output();
    assert!(
        made.status.success(),
        "[{tag}] mke2fs: {}",
        String::from_utf8_lossy(&made.stderr)
    );

    assert_eq!(
        inode_geometry(&ours),
        inode_geometry(&theirs),
        "[{tag}] (inode count, inodes per group): ours, then mke2fs's"
    );
    let _ = std::fs::remove_file(&ours);
    let _ = std::fs::remove_file(&theirs);
}

#[test]
fn two_groups_at_4_kib() {
    matches_mke2fs("two_groups", FsFlavor::Ext4, 4096, 256 << 20, 5000);
}

#[test]
fn one_group_exact() {
    matches_mke2fs("exact", FsFlavor::Ext4, 4096, 32 << 20, 4096);
}

#[test]
fn one_kib_blocks() {
    matches_mke2fs("one_kib", FsFlavor::Ext4, 1024, 8 << 20, 100);
}

#[test]
fn ext2_one_group() {
    matches_mke2fs("ext2_1g", FsFlavor::Ext2, 4096, 32 << 20, 1000);
}

#[test]
fn ext3_one_group() {
    matches_mke2fs("ext3_1g", FsFlavor::Ext3, 4096, 32 << 20, 1000);
}

/// Up to whole table blocks, then down to whole bitmap bytes: fewer than
/// asked where a table block holds fewer than eight inodes.
#[test]
fn one_kib_blocks_round_down_to_bitmap_bytes() {
    matches_mke2fs("one_kib_108", FsFlavor::Ext4, 1024, 8 << 20, 108);
    matches_mke2fs("one_kib_2500", FsFlavor::Ext4, 1024, 8 << 20, 2500);
}

/// A group holds at most 65536 inodes less one table block's worth.
#[test]
fn a_request_past_the_per_group_limit_is_clamped() {
    matches_mke2fs("clamp_16k", FsFlavor::Ext4, 16384, 32 << 20, 70000);
    matches_mke2fs("clamp_64k", FsFlavor::Ext4, 65536, 128 << 20, 70000);
    matches_mke2fs("clamp_ext2", FsFlavor::Ext2, 16384, 32 << 20, 70000);
}
