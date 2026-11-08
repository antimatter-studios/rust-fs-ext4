//! The default inode count, and the one a bytes-per-inode ratio gives,
//! held to `mke2fs` in the guest.
//!
//! `tests/mkfs_default_inode_count.rs` and `tests/mkfs_bytes_per_inode.rs`
//! state the counts this crate chooses. This proves they are the counts
//! `mke2fs` chooses: for each case both format the same size, block size
//! and inode size without an inode count, and `dumpe2fs -h` must report
//! the same `Inode count` and `Inodes per group` for both. The volume this
//! crate made must pass `e2fsck -fn`.
//!
//! `-I` is given because `mke2fs` picks 128-byte inodes for volumes under
//! 512 MiB and this crate's ext4 inodes are 256 bytes: the count must not
//! depend on it, and the rounding to whole table blocks does.

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::features::FsFlavor;
use fs_ext4::mkfs::{self, FormatOptions};
use fs_ext4_test_support::{assert_e2fsck_clean, oracle, temp_path};

fn scratch(tag: &str, who: &str, size: u64) -> String {
    let path = temp_path!("fs_ext4_ratio_{who}_{tag}_{}.img", std::process::id());
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

fn matches_mke2fs(
    tag: &str,
    flavor: FsFlavor,
    block_size: u32,
    size: u64,
    bytes_per_inode: Option<u32>,
) {
    let (kind, inode_size) = match flavor {
        FsFlavor::Ext2 => ("ext2", "128"),
        FsFlavor::Ext3 => ("ext3", "128"),
        FsFlavor::Ext4 => ("ext4", "256"),
    };

    let ours = scratch(tag, "ours", size);
    {
        let dev = FileDevice::open_rw(&ours).expect("open_rw");
        let opts = FormatOptions {
            bytes_per_inode,
            ..FormatOptions::new(flavor)
        };
        mkfs::format_filesystem_with_options(&dev, None, None, size, block_size, &opts)
            .unwrap_or_else(|e| panic!("[{tag}] format: {e:?}"));
        dev.flush().expect("flush");
    }
    assert_e2fsck_clean(&ours, tag);

    let theirs = scratch(tag, "mke2fs", size);
    let mut args: Vec<String> = [
        "-q",
        "-F",
        "-t",
        kind,
        "-O",
        "^has_journal",
        "-I",
        inode_size,
    ]
    .map(String::from)
    .into();
    args.extend(["-b".into(), block_size.to_string()]);
    if let Some(ratio) = bytes_per_inode {
        args.extend(["-i".into(), ratio.to_string()]);
    }
    let made = oracle("mke2fs").args(&args).arg(&theirs).output();
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
fn one_gib_at_4_kib_blocks() {
    matches_mke2fs("4k_1g", FsFlavor::Ext4, 4096, 1 << 30, None);
}

#[test]
fn one_gib_at_2_kib_blocks() {
    matches_mke2fs("2k_1g", FsFlavor::Ext4, 2048, 1 << 30, None);
}

#[test]
fn just_under_512_mib() {
    matches_mke2fs("4k_511m", FsFlavor::Ext4, 4096, 511 << 20, None);
}

#[test]
fn exactly_512_mib() {
    matches_mke2fs("4k_512m", FsFlavor::Ext4, 4096, 512 << 20, None);
}

#[test]
fn eight_mib_at_1_kib_blocks() {
    matches_mke2fs("1k_8m", FsFlavor::Ext4, 1024, 8 << 20, None);
}

#[test]
fn two_mib() {
    matches_mke2fs("4k_2m", FsFlavor::Ext4, 4096, 2 << 20, None);
}

#[test]
fn ext2_eight_mib_at_1_kib_blocks() {
    matches_mke2fs("ext2_1k_8m", FsFlavor::Ext2, 1024, 8 << 20, None);
}

#[test]
fn a_ratio_of_64_kib() {
    matches_mke2fs("4k_1g_i64k", FsFlavor::Ext4, 4096, 1 << 30, Some(64 << 10));
}

/// The default ratio is raised to the block size: one inode per block.
#[test]
fn the_default_ratio_is_never_below_the_block_size() {
    matches_mke2fs("8k_256m", FsFlavor::Ext4, 8192, 256 << 20, None);
    matches_mke2fs("16k_256m", FsFlavor::Ext4, 16384, 256 << 20, None);
    matches_mke2fs("64k_2g", FsFlavor::Ext4, 65536, 98304 * 65536, None);
}

/// A ratio the caller chooses is not raised: at 64 KiB blocks, one inode
/// per 16 KiB reaches the per-group limit.
#[test]
fn a_chosen_ratio_below_the_block_size_is_taken_as_given() {
    matches_mke2fs(
        "64k_2g_i16k",
        FsFlavor::Ext4,
        65536,
        98304 * 65536,
        Some(16 << 10),
    );
}
