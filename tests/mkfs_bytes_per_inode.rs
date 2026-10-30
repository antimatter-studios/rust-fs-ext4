//! A caller-chosen bytes-per-inode ratio, as `mke2fs -i` takes one.
//!
//! The volume gets one inode for every `bytes_per_inode` bytes, spread and
//! rounded as any count is. An explicit inode count, as `-N` gives one,
//! takes precedence over the ratio. A ratio outside 1 KiB..=64 MiB, or one
//! that needs more inodes in a group than its bitmap block can track, is
//! refused before anything is written.
//!
//! Sparse in-memory volumes: no fixture, no VM.

use fs_ext4::error::{Error, Result};
use fs_ext4::features::FsFlavor;
use fs_ext4::mkfs::{self, FormatOptions};
use fs_ext4::Filesystem;
use std::sync::Arc;

mod sparse_device;
use sparse_device::Sparse;

const GIB: u64 = 1 << 30;

fn format(inodes: Option<u32>, bytes_per_inode: Option<u32>) -> (Arc<Sparse>, Result<()>) {
    let dev = Sparse::new(GIB);
    let opts = FormatOptions {
        inodes_count: inodes,
        bytes_per_inode,
        ..FormatOptions::new(FsFlavor::Ext4)
    };
    let r =
        mkfs::format_filesystem_with_options(dev.as_ref(), None, Some([8; 16]), GIB, 4096, &opts);
    (dev, r)
}

fn inode_geometry(dev: &Arc<Sparse>) -> (u32, u32) {
    let fs = Filesystem::mount(dev.clone()).expect("mount");
    (fs.sb.inodes_per_group, fs.sb.inodes_count)
}

#[test]
fn a_ratio_sets_the_inode_count() {
    // 1 GiB / 64 KiB = 16384, over 8 groups: 2048 each.
    let (dev, r) = format(None, Some(64 << 10));
    r.expect("format");
    assert_eq!(inode_geometry(&dev), (2048, 16384));
}

#[test]
fn an_inode_count_takes_precedence_over_a_ratio() {
    // 1000 over 8 groups is 125, rounded up to sixteen 256-byte inodes a
    // 4 KiB table block: 128 each.
    let (dev, r) = format(Some(1000), Some(64 << 10));
    r.expect("format");
    assert_eq!(inode_geometry(&dev), (128, 1024));
}

#[test]
fn a_ratio_out_of_range_is_refused_and_writes_nothing() {
    for bad in [0, 512, 1023, (64 << 20) + 1] {
        let (dev, r) = format(None, Some(bad));
        assert!(matches!(r, Err(Error::InvalidArgument(_))), "{bad}: {r:?}");
        assert_eq!(
            dev.written_end(),
            0,
            "{bad}: a refused format writes nothing"
        );
    }
}

#[test]
fn a_ratio_needing_more_inodes_than_a_bitmap_holds_is_refused() {
    // 1 GiB / 1 KiB is 131072 inodes a group, and a 4 KiB bitmap block
    // tracks 32768.
    let (dev, r) = format(None, Some(1024));
    assert!(matches!(r, Err(Error::InvalidArgument(_))), "{r:?}");
    assert_eq!(dev.written_end(), 0);
}

#[test]
fn the_limits_of_the_range_are_accepted() {
    // 1 GiB / 64 MiB is 16 inodes, 2 a group, raised to the twelve group 0
    // needs and rounded to a whole table block: 16 each.
    let (dev, r) = format(None, Some(64 << 20));
    r.expect("format at 64 MiB per inode");
    assert_eq!(inode_geometry(&dev), (16, 128));
}
