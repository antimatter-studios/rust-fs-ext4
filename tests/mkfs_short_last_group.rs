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

use fs_ext4::block_io::BlockDevice;
use fs_ext4::error::{Error, Result};
use fs_ext4::features::FsFlavor;
use fs_ext4::mkfs::{self, FormatOptions};
use fs_ext4::Filesystem;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const BS: u32 = 4096;
/// Blocks in a full group at 4 KiB: one bitmap block's bits.
const BPG: u64 = 32768;
const CHUNK: u64 = 4096;

/// A device of `size` bytes that stores only the chunks written to it, so
/// a volume of several groups costs what its metadata costs.
struct Sparse {
    size: u64,
    chunks: Mutex<BTreeMap<u64, Vec<u8>>>,
}

impl Sparse {
    fn new(size: u64) -> Arc<Self> {
        Arc::new(Self {
            size,
            chunks: Mutex::new(BTreeMap::new()),
        })
    }

    /// One past the last byte anything has been written to.
    fn written_end(&self) -> u64 {
        self.chunks
            .lock()
            .unwrap()
            .keys()
            .next_back()
            .map_or(0, |c| (c + 1) * CHUNK)
    }
}

impl BlockDevice for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        assert!(offset + buf.len() as u64 <= self.size, "read past the end");
        let chunks = self.chunks.lock().unwrap();
        for (i, byte) in buf.iter_mut().enumerate() {
            let at = offset + i as u64;
            *byte = chunks
                .get(&(at / CHUNK))
                .map_or(0, |c| c[(at % CHUNK) as usize]);
        }
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.size
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        assert!(offset + buf.len() as u64 <= self.size, "write past the end");
        let mut chunks = self.chunks.lock().unwrap();
        for (i, &byte) in buf.iter().enumerate() {
            let at = offset + i as u64;
            chunks
                .entry(at / CHUNK)
                .or_insert_with(|| vec![0; CHUNK as usize])[(at % CHUNK) as usize] = byte;
        }
        Ok(())
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

fn format(blocks: u64, inodes: Option<u32>) -> (Arc<Sparse>, Result<()>) {
    let size = blocks * u64::from(BS);
    let dev = Sparse::new(size);
    let opts = FormatOptions {
        flavor: FsFlavor::Ext4,
        inodes_count: inodes,
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
    formats_as("1g+1_default", BPG + 1, None, BPG, 8192);
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
    // Group 0 needs 521 blocks: the superblock, the descriptor table, two
    // bitmaps, 8192 inodes' 512-block table, the root's block and
    // lost+found's four. There is no group before it to fall back to.
    let (dev, r) = format(512, None);
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
