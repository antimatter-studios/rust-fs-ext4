//! A short last block group, held to `mke2fs` in the guest.
//!
//! `tests/mkfs_short_last_group.rs` states when this crate leaves a short
//! last group off the volume. This proves it is when `mke2fs` does: for
//! each device size both format it with the same block size, inode size
//! and inode count, and `dumpe2fs -h` must report the same `Block count`
//! for both. Each size is either side of the threshold, or far below it.
//! The volume this crate made must pass `e2fsck -fn` and hold at least
//! the inodes asked for.
//!
//! `mke2fs` is asked for the layout this crate writes, because a group's
//! metadata is what its short tail is measured against:
//!
//! - `-I 256`, because a device this small falls in `mke2fs`'s
//!   small-volume profile, which picks 128-byte inodes, and this crate's
//!   ext4 inodes are 256 bytes.
//! - `-O ^resize_inode`, because by default `mke2fs` reserves descriptor
//!   table blocks for growing the volume in every group that carries a
//!   superblock backup -- 48 of them at these sizes -- and counts them
//!   against that group. This crate reserves none. With them, `mke2fs`
//!   keeps a 3-group-plus tail of 571 blocks and drops 570; without them,
//!   523 and 522, which is this crate's threshold.

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::features::FsFlavor;
use fs_ext4::mkfs::{self, FormatOptions};
use fs_ext4_test_support::{assert_e2fsck_clean, oracle, temp_path};

const BS: u64 = 4096;
const BPG: u64 = 32768;

fn scratch(tag: &str, who: &str, size: u64) -> String {
    let path = temp_path!("fs_ext4_short_group_{who}_{tag}_{}.img", std::process::id());
    std::fs::File::create(&path)
        .and_then(|f| f.set_len(size))
        .unwrap_or_else(|e| panic!("create {path}: {e}"));
    path
}

/// A `Name: value` field of `dumpe2fs -h`.
fn header(image: &str, name: &str) -> u64 {
    let out = oracle("dumpe2fs")
        .args(["-h", image])
        .judged()
        .clean("dumpe2fs");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
        .unwrap_or_else(|| panic!("dumpe2fs -h {image}: no {name:?} in\n{text}"))
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("dumpe2fs -h {image}: {name}: {e}"))
}

fn matches_mke2fs(tag: &str, blocks: u64, inodes: u32) {
    let size = blocks * BS;

    let ours = scratch(tag, "ours", size);
    {
        let dev = FileDevice::open_rw(&ours).expect("open_rw");
        let opts = FormatOptions {
            inodes_count: Some(inodes),
            ..FormatOptions::new(FsFlavor::Ext4)
        };
        mkfs::format_filesystem_with_options(&dev, None, None, size, BS as u32, &opts)
            .unwrap_or_else(|e| panic!("[{tag}] format: {e:?}"));
        dev.flush().expect("flush");
    }
    assert_e2fsck_clean(&ours, tag);
    assert!(
        header(&ours, "Inode count") >= u64::from(inodes),
        "[{tag}] at least the inodes asked for"
    );

    let theirs = scratch(tag, "mke2fs", size);
    let made = oracle("mke2fs")
        .args([
            "-q",
            "-F",
            "-t",
            "ext4",
            "-O",
            "^has_journal,^resize_inode",
            "-b",
            "4096",
            "-I",
            "256",
            "-N",
        ])
        .arg(inodes.to_string())
        .arg(&theirs)
        .output();
    assert!(
        made.status.success(),
        "[{tag}] mke2fs: {}",
        String::from_utf8_lossy(&made.stderr)
    );

    assert_eq!(
        header(&ours, "Block count"),
        header(&theirs, "Block count"),
        "[{tag}] block count of a {blocks}-block device: ours, then mke2fs's"
    );
    let _ = std::fs::remove_file(&ours);
    let _ = std::fs::remove_file(&theirs);
}

#[test]
fn one_full_group_and_one_block() {
    matches_mke2fs("1g_1", BPG + 1, 2000);
}

#[test]
fn four_full_groups_and_300_blocks() {
    matches_mke2fs("4g_300", 4 * BPG + 300, 30000);
}

#[test]
fn a_last_group_at_the_threshold() {
    matches_mke2fs("4g_427", 4 * BPG + 427, 30000);
}

#[test]
fn a_last_group_one_block_below_the_threshold() {
    matches_mke2fs("4g_426", 4 * BPG + 426, 30000);
}

/// Group 3 carries a superblock backup, which counts against the threshold.
#[test]
fn a_last_group_with_a_backup_at_the_threshold() {
    matches_mke2fs("3g_523", 3 * BPG + 523, 30000);
}

#[test]
fn a_last_group_with_a_backup_one_block_below_the_threshold() {
    matches_mke2fs("3g_522", 3 * BPG + 522, 30000);
}
