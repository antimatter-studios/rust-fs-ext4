//! The inode count a caller asks the formatter for, as `mke2fs -N` takes
//! one.
//!
//! A request of N inodes is spread over the groups and rounded up: each
//! group gets `ceil(N / groups)` inodes, rounded up to fill whole
//! inode-table blocks and whole inode-bitmap bytes, so the volume has at
//! least N and never a partial table block. A request the layout cannot
//! hold -- more inodes than a group's bitmap block has bits, or an inode
//! table larger than a group -- is refused before anything is written.
//! `tests/mkfs_inode_count_oracle.rs` holds the rounding to `mke2fs -N`.
//!
//! In-memory volumes: no fixture, no VM.

use fs_ext4::block_io::BlockDevice;
use fs_ext4::error::{Error, Result};
use fs_ext4::features::FsFlavor;
use fs_ext4::mkfs::{self, FormatOptions};
use fs_ext4::Filesystem;
use std::sync::{Arc, Mutex};

struct MemDev {
    bytes: Mutex<Vec<u8>>,
}

impl BlockDevice for MemDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let b = self.bytes.lock().unwrap();
        let start = offset as usize;
        buf.copy_from_slice(&b[start..start + buf.len()]);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.bytes.lock().unwrap().len() as u64
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut b = self.bytes.lock().unwrap();
        let start = offset as usize;
        b[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

fn device(size: u64) -> Arc<MemDev> {
    Arc::new(MemDev {
        bytes: Mutex::new(vec![0u8; size as usize]),
    })
}

fn format(
    dev: &MemDev,
    flavor: FsFlavor,
    block_size: u32,
    inodes_count: Option<u32>,
) -> Result<()> {
    let size = dev.size_bytes();
    let opts = FormatOptions {
        flavor,
        inodes_count,
    };
    mkfs::format_filesystem_with_options(dev, None, Some([5; 16]), size, block_size, &opts)
}

/// Format with `inodes_count` and check what the superblock says, that the
/// free count leaves the eleven in use, and that the volume is whole: its
/// own audit is clean and a file can be created in it.
fn formats_with(
    tag: &str,
    flavor: FsFlavor,
    block_size: u32,
    size: u64,
    requested: u32,
    expect_per_group: u32,
    expect_groups: u32,
) {
    let dev = device(size);
    format(&dev, flavor, block_size, Some(requested)).unwrap_or_else(|e| panic!("[{tag}] {e:?}"));
    let fs = Filesystem::mount(dev.clone()).expect("mount");
    assert_eq!(fs.groups.len() as u32, expect_groups, "[{tag}] groups");
    assert_eq!(
        fs.sb.inodes_per_group, expect_per_group,
        "[{tag}] inodes per group"
    );
    assert_eq!(
        fs.sb.inodes_count,
        expect_per_group * expect_groups,
        "[{tag}] inode count"
    );
    assert!(fs.sb.inodes_count >= requested, "[{tag}] at least as asked");
    assert_eq!(
        u64::from(fs.sb.free_inodes_count),
        u64::from(fs.sb.inodes_count) - 11,
        "[{tag}] free inodes: all but the reserved ten and lost+found"
    );
    let report = fs_ext4::fsck::audit(&fs, u32::MAX, u32::MAX).expect("audit");
    assert!(report.is_clean(), "[{tag}] {:?}", report.anomalies);
    let ino = fs.apply_create("/f", 0o644).expect("create a file");
    assert_eq!(ino, 12, "[{tag}] the first free inode");
}

#[test]
fn a_requested_count_is_spread_over_the_groups_and_rounded_up() {
    // 256 MiB at 4 KiB: two groups. 5000 / 2 = 2500, rounded up to a
    // whole number of 256-byte inodes per 4 KiB block (16): 2512.
    formats_with("two_groups", FsFlavor::Ext4, 4096, 256 << 20, 5000, 2512, 2);
    // One group of 128-byte inodes, 32 to a block: 1000 becomes 1024.
    formats_with("ext2_1g", FsFlavor::Ext2, 4096, 32 << 20, 1000, 1024, 1);
    formats_with("ext3_1g", FsFlavor::Ext3, 4096, 32 << 20, 1000, 1024, 1);
    // 1 KiB blocks hold four 256-byte inodes, but a bitmap byte covers
    // eight: 100 becomes 104.
    formats_with("one_kib", FsFlavor::Ext4, 1024, 8 << 20, 100, 104, 1);
    // An exact fit is kept as asked.
    formats_with("exact", FsFlavor::Ext4, 4096, 32 << 20, 4096, 4096, 1);
}

#[test]
fn a_tiny_request_still_leaves_room_past_the_reserved_inodes() {
    // Group 0 holds the ten reserved inodes and lost+found, so at least
    // twelve are needed; rounded to a whole table block, sixteen.
    formats_with("tiny", FsFlavor::Ext4, 4096, 32 << 20, 1, 16, 1);
}

#[test]
fn without_a_request_the_default_is_unchanged() {
    let dev = device(256 << 20);
    format(&dev, FsFlavor::Ext4, 4096, None).expect("format");
    let fs = Filesystem::mount(dev).expect("mount");
    assert_eq!(fs.sb.inodes_per_group, 8192);
    assert_eq!(fs.sb.inodes_count, 2 * 8192);
}

/// A refused request leaves every byte of the device as it was.
fn refused(tag: &str, flavor: FsFlavor, block_size: u32, size: u64, requested: u32) {
    let dev = device(size);
    let err = format(&dev, flavor, block_size, Some(requested))
        .expect_err(&format!("[{tag}] {requested} inodes must be refused"));
    assert!(matches!(err, Error::InvalidArgument(_)), "[{tag}] {err:?}");
    assert!(
        dev.bytes.lock().unwrap().iter().all(|&b| b == 0),
        "[{tag}] a refused format writes nothing"
    );
}

#[test]
fn zero_inodes_is_refused() {
    refused("zero", FsFlavor::Ext4, 4096, 32 << 20, 0);
}

#[test]
fn more_inodes_than_a_bitmap_block_holds_is_refused() {
    // One 4 KiB bitmap block has 32768 bits: one group cannot hold more.
    refused("bitmap", FsFlavor::Ext4, 4096, 32 << 20, 32769);
}

#[test]
fn an_inode_table_larger_than_its_group_is_refused() {
    // 4 MiB at 4 KiB is 1024 blocks; 32768 inodes of 128 bytes need 1024
    // table blocks, leaving nothing for the bitmaps and the root.
    refused("table", FsFlavor::Ext2, 4096, 4 << 20, 32768);
}

#[test]
fn a_short_final_group_too_small_for_its_table_is_refused_before_writing() {
    // A full group and a 600-block one: 40000 inodes over two groups is a
    // 1250-block table per group, which the full group holds and the last
    // cannot. Nothing may be written, the first group's metadata included.
    refused(
        "short_last",
        FsFlavor::Ext4,
        4096,
        (32768 + 600) * 4096,
        40000,
    );
}
