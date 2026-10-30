//! How many inodes a volume gets when the caller does not say.
//!
//! `mke2fs` sizes it from the volume: one inode for every so many bytes,
//! a ratio that depends on how large the volume is. Below 3 MiB it is one
//! per 8 KiB, below 512 MiB one per 4 KiB, and from there one per 16 KiB.
//! The ratio is never below the block size, so by default a volume never
//! has more inodes than blocks; a ratio the caller chooses is taken as
//! given. The count is then spread over the groups and rounded and clamped
//! as an explicit count is (`tests/mkfs_inode_count.rs`). A fixed 8192 a
//! group, which this formatter used, agrees with that only at 4 KiB
//! blocks on volumes of 512 MiB and more: at 1 KiB blocks it spends a
//! quarter of the volume on inode tables, and a full 64 KiB group gets
//! 8192 where `mke2fs` gives 65280. `tests/mkfs_default_inode_count_oracle.rs`
//! holds each count here to `mke2fs` itself.
//!
//! Sparse in-memory volumes: no fixture, no VM.

use fs_ext4::features::FsFlavor;
use fs_ext4::mkfs::{self, FormatOptions};
use fs_ext4::Filesystem;

mod sparse_device;
use sparse_device::Sparse;

fn formats_with(
    tag: &str,
    flavor: FsFlavor,
    block_size: u32,
    size: u64,
    expect_per_group: u32,
    expect_groups: u32,
) {
    let dev = Sparse::new(size);
    mkfs::format_filesystem_with_options(
        dev.as_ref(),
        None,
        Some([6; 16]),
        size,
        block_size,
        &FormatOptions::new(flavor),
    )
    .unwrap_or_else(|e| panic!("[{tag}] format: {e:?}"));
    let fs = Filesystem::mount(dev.clone()).expect("mount");
    assert_eq!(fs.groups.len() as u32, expect_groups, "[{tag}] groups");
    assert_eq!(
        (fs.sb.inodes_per_group, fs.sb.inodes_count),
        (expect_per_group, expect_per_group * expect_groups),
        "[{tag}] (inodes per group, inode count)"
    );
    let report = fs_ext4::fsck::audit(&fs, u32::MAX, u32::MAX).expect("audit");
    assert!(report.is_clean(), "[{tag}] {:?}", report.anomalies);
    fs.apply_create("/f", 0o644).expect("create a file");
}

#[test]
fn a_volume_of_512_mib_or_more_gets_one_inode_per_16_kib() {
    // 1 GiB / 16 KiB = 65536, over 8 groups of 4 KiB blocks: 8192 each,
    // the count the fixed default gave.
    formats_with("4k_1g", FsFlavor::Ext4, 4096, 1 << 30, 8192, 8);
    // At 2 KiB blocks the same volume is 32 groups: 2048 each, not 8192.
    formats_with("2k_1g", FsFlavor::Ext4, 2048, 1 << 30, 2048, 32);
}

#[test]
fn a_volume_under_512_mib_gets_one_inode_per_4_kib() {
    // 256 MiB / 4 KiB = 65536, over 2 groups: 32768 each.
    formats_with("4k_256m", FsFlavor::Ext4, 4096, 256 << 20, 32768, 2);
    // 8 MiB / 4 KiB = 2048, in the one group 1 KiB blocks allow.
    formats_with("1k_8m", FsFlavor::Ext4, 1024, 8 << 20, 2048, 1);
    formats_with("ext2_1k_8m", FsFlavor::Ext2, 1024, 8 << 20, 2048, 1);
    // 32 MiB / 4 KiB = 8192: the fixed default again, by coincidence.
    formats_with("ext2_4k_32m", FsFlavor::Ext2, 4096, 32 << 20, 8192, 1);
}

#[test]
fn a_volume_under_3_mib_gets_one_inode_per_8_kib() {
    // 2 MiB / 8 KiB = 256. A fixed 8192 needed a 512-block table, the
    // whole device, and was refused.
    formats_with("4k_2m", FsFlavor::Ext4, 4096, 2 << 20, 256, 1);
}

#[test]
fn the_default_ratio_is_never_below_the_block_size() {
    // 256 MiB is under 512 MiB, one inode per 4 KiB by size, but 8 KiB
    // and 16 KiB blocks raise it to one per block: 32768 and 16384.
    formats_with("8k_256m", FsFlavor::Ext4, 8192, 256 << 20, 32768, 1);
    formats_with("16k_256m", FsFlavor::Ext4, 16384, 256 << 20, 16384, 1);
    // 98304 blocks of 64 KiB, two groups: one inode per block, 49152 a
    // group, not one per 16 KiB.
    formats_with("64k_2g", FsFlavor::Ext4, 65536, 98304 * 65536, 49152, 2);
}

#[test]
fn a_full_group_of_large_blocks_is_clamped_to_the_per_group_limit() {
    // One full 16 KiB group, 65528 blocks at one inode per block, is past
    // 65536 inodes less one table block's worth: 65472.
    formats_with("16k_1g", FsFlavor::Ext4, 16384, 65528 * 16384, 65472, 1);
}
