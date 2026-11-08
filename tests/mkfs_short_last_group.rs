//! A last block group too small to be worth having is left off the volume,
//! as `mke2fs` leaves it off.
//!
//! A device whose size is not a whole number of groups ends in a short
//! group, which needs its own bitmaps and inode table. When it cannot hold
//! them and 50 blocks of data besides, `mke2fs` formats the whole groups
//! before it and leaves the device's last blocks unused: the volume is a
//! little smaller than the device, and the superblock's block count says
//! by how much. `tests/mkfs_short_last_group_oracle.rs` holds the block
//! count and the threshold to `mke2fs` itself.
//!
//! Dropping a group changes how a requested inode count is spread, and so
//! every group's inode table: the layout is worked out again until it is
//! stable, and still holds at least the inodes asked for.
//!
//! Sparse in-memory volumes: no fixture, no VM.

use fs_ext4::error::{Error, Result};
use fs_ext4::features::FsFlavor;
use fs_ext4::mkfs::{self, FormatOptions};
use fs_ext4::Filesystem;
use std::sync::Arc;

mod sparse_device;
use sparse_device::Sparse;

const BS: u32 = 4096;
/// Blocks in a full group at 4 KiB: one bitmap block's bits.
const BPG: u64 = 32768;
fn format(blocks: u64, inodes: Option<u32>) -> (Arc<Sparse>, Result<()>) {
    let size = blocks * u64::from(BS);
    let dev = Sparse::new(size);
    let opts = FormatOptions {
        inodes_count: inodes,
        ..FormatOptions::new(FsFlavor::Ext4)
    };
    let r =
        mkfs::format_filesystem_with_options(dev.as_ref(), None, Some([4; 16]), size, BS, &opts);
    (dev, r)
}

/// Format a device of `blocks` blocks and require the volume to be
/// `expect_blocks` long, in `expect_blocks / BPG` (rounded up) groups,
/// holding `expect_inodes` inodes, whole by its own audit, writable, and
/// with nothing written past its end.
fn formats_as(tag: &str, blocks: u64, inodes: Option<u32>, expect_blocks: u64, expect_inodes: u32) {
    let (dev, r) = format(blocks, inodes);
    r.unwrap_or_else(|e| panic!("[{tag}] format a {blocks}-block device: {e:?}"));
    let fs = Filesystem::mount(dev.clone()).expect("mount");
    assert_eq!(fs.sb.blocks_count, expect_blocks, "[{tag}] block count");
    assert_eq!(
        fs.groups.len() as u64,
        expect_blocks.div_ceil(BPG),
        "[{tag}] groups"
    );
    assert_eq!(fs.sb.inodes_count, expect_inodes, "[{tag}] inode count");
    if let Some(n) = inodes {
        assert!(
            fs.sb.inodes_count >= n,
            "[{tag}] at least the inodes asked for"
        );
    }
    let report = fs_ext4::fsck::audit(&fs, u32::MAX, u32::MAX).expect("audit");
    assert!(report.is_clean(), "[{tag}] {:?}", report.anomalies);
    fs.apply_create("/f", 0o644).expect("create a file");
    fs.apply_pwrite("/f", 0, &[0x5a; 8192])
        .expect("write to it");
    drop(fs);
    assert!(
        dev.written_end() <= expect_blocks * u64::from(BS),
        "[{tag}] nothing is written past the volume's last block"
    );
}

#[test]
fn one_full_group_and_one_block_formats_as_one_group() {
    formats_as("1g+1", BPG + 1, Some(2000), BPG, 2000);
    // Without a count: one group of 128 MiB, under 512 MiB, gets one
    // inode per 4 KiB, worked out again after the drop.
    formats_as("1g+1_default", BPG + 1, None, BPG, 32768);
}

#[test]
fn four_full_groups_and_300_blocks_formats_as_four_groups() {
    // Five groups would be 6000 inodes each, a 375-block table: group 4
    // (no superblock backup) needs 2 bitmaps + 375 + 50 data = 427 blocks
    // and has 300. Four groups are 7500 each, rounded up to whole 4 KiB
    // table blocks of sixteen 256-byte inodes: 7504, 30016 in all.
    formats_as("4g+300", 4 * BPG + 300, Some(30000), 4 * BPG, 30016);
}

#[test]
fn a_last_group_at_the_threshold_is_kept() {
    formats_as("4g+427", 4 * BPG + 427, Some(30000), 4 * BPG + 427, 30000);
}

#[test]
fn a_last_group_one_block_below_the_threshold_is_dropped() {
    formats_as("4g+426", 4 * BPG + 426, Some(30000), 4 * BPG, 30016);
}

#[test]
fn a_device_too_small_for_group_0_is_still_refused() {
    // With 8192 inodes group 0 needs 521 blocks: the superblock, the
    // descriptor table, two bitmaps, a 512-block inode table, the root's
    // block and lost+found's four. There is no group before it to fall
    // back to. (Without a count, 512 blocks get 256 inodes, and fit.)
    let (dev, r) = format(512, Some(8192));
    assert!(matches!(r, Err(Error::InvalidArgument(_))), "{r:?}");
    assert_eq!(dev.written_end(), 0, "a refused format writes nothing");
}

/// Group 3 carries a superblock backup and a descriptor table copy, which
/// count against it: four groups are 7504 inodes each (a 469-block table),
/// so it needs 1 + 1 + 2 + 469 + 50 = 523 blocks.
#[test]
fn a_last_group_with_a_backup_counts_it_against_the_threshold() {
    formats_as("3g+523", 3 * BPG + 523, Some(30000), 3 * BPG + 523, 30016);
    // Three groups are 10000 inodes each.
    formats_as("3g+522", 3 * BPG + 522, Some(30000), 3 * BPG, 30000);
}
