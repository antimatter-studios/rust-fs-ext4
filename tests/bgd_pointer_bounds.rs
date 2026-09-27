//! A group descriptor's three pointers are bounded below as well as above
//! (#320).
//!
//! `bg_block_bitmap`, `bg_inode_bitmap` and `bg_inode_table` were checked
//! only against the end of the filesystem. A pointer of 0 -- or 1 on a
//! 1 KiB volume -- names the primary superblock or the descriptor table, and
//! every one of the three is a write target: the bitmap writers put a whole
//! block there and inode writes put an inode image there, over the metadata
//! every mount reads first.
//!
//! The rule is the one the kernel's `ext4_check_descriptors` applies: no
//! pointer inside group 0's superblock and descriptor table, and, without
//! `FLEX_BG`, no pointer outside the descriptor's own group. The volumes are
//! the crate's own `mkfs`, which lays groups out the classic, non-flex_bg
//! way; the flex_bg layouts `mkfs.ext4` writes are the repository's
//! committed fixtures, which the images tier mounts through the same check.

use fs_ext4::block_io::BlockDevice;
use fs_ext4::checksum::{group_desc_csum, Checksummer};
use fs_ext4::error::{Error, Result};
use fs_ext4::features::FsFlavor;
use fs_ext4::fs::Filesystem;
use fs_ext4::mkfs;
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
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut b = self.bytes.lock().unwrap();
        let start = offset as usize;
        b[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.bytes.lock().unwrap().len() as u64
    }
    fn is_writable(&self) -> bool {
        true
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// Descriptor field offsets (lo halves), from the ext4 on-disk layout.
#[derive(Clone, Copy, Debug)]
enum Pointer {
    BlockBitmap = 0x00,
    InodeBitmap = 0x04,
    InodeTable = 0x08,
}

const POINTERS: [Pointer; 3] = [
    Pointer::BlockBitmap,
    Pointer::InodeBitmap,
    Pointer::InodeTable,
];

/// The two layouts the crate's `mkfs` writes: 2 KiB ext4 across three
/// groups, and 1 KiB ext2, where block 0 is the boot block and the
/// superblock is block 1.
#[derive(Clone, Copy, Debug)]
enum Volume {
    Ext4TwoKiBThreeGroups,
    Ext2OneKiB,
}

fn format(volume: Volume) -> Arc<MemDev> {
    let (bytes, block_size, flavor) = match volume {
        Volume::Ext4TwoKiBThreeGroups => (80u64 << 20, 2048, FsFlavor::Ext4),
        Volume::Ext2OneKiB => (8u64 << 20, 1024, FsFlavor::Ext2),
    };
    let dev = Arc::new(MemDev {
        bytes: Mutex::new(vec![0u8; bytes as usize]),
    });
    mkfs::format_filesystem_with_flavor(dev.as_ref(), None, None, bytes, block_size, flavor)
        .expect("format");
    dev
}

/// Set one pointer of `group`'s descriptor to `value` (hi half zeroed) and
/// restamp the descriptor's checksum, so the pointer is what gets judged.
fn set_pointer(dev: &Arc<MemDev>, group: u64, pointer: Pointer, value: u32) {
    let fs = Filesystem::mount(dev.clone()).expect("the volume as formatted mounts");
    let sb = fs.sb.clone();
    drop(fs);
    let bs = u64::from(sb.block_size());
    let (block, off) = sb.descriptor_location(group);
    let at = block * bs + off as u64;
    let mut desc = vec![0u8; sb.desc_size as usize];
    dev.read_at(at, &mut desc).unwrap();
    let field = pointer as usize;
    desc[field..field + 4].copy_from_slice(&value.to_le_bytes());
    if desc.len() >= 64 {
        desc[0x20 + field..0x24 + field].copy_from_slice(&0u32.to_le_bytes());
    }
    let csum = Checksummer::from_superblock(&sb);
    if let Some(c) = group_desc_csum(&sb, &csum, group as u32, &desc) {
        desc[0x1E..0x20].copy_from_slice(&c.to_le_bytes());
    }
    dev.write_at(at, &desc).unwrap();
}

fn assert_refused(dev: Arc<MemDev>, what: &str, because: &str) {
    match Filesystem::mount(dev) {
        Err(Error::Corrupt(why)) => assert!(
            why.contains(because),
            "{what}: refused, but not by the descriptor bound: {why}"
        ),
        Err(other) => panic!("{what}: wrong error {other:?}"),
        Ok(_) => panic!("{what}: mounted"),
    }
}

const INTO_GROUP_ZERO: &str = "points into group 0's superblock or descriptor table";
const OUT_OF_ITS_GROUP: &str = "points outside its own group";

#[test]
fn the_volumes_as_formatted_mount() {
    for volume in [Volume::Ext4TwoKiBThreeGroups, Volume::Ext2OneKiB] {
        let fs = Filesystem::mount(format(volume))
            .unwrap_or_else(|e| panic!("{volume:?} as formatted: {e:?}"));
        if matches!(volume, Volume::Ext4TwoKiBThreeGroups) {
            assert_eq!(fs.sb.block_group_count(), 3, "fixture: three groups");
        }
    }
}

/// The issue's red: group 0's pointer set to 0. On a 2 KiB or larger volume
/// block 0 holds the primary superblock; on a 1 KiB one it is the boot
/// block, below `s_first_data_block`, and outside every group.
#[test]
fn a_group_0_pointer_of_zero_is_refused() {
    for volume in [Volume::Ext4TwoKiBThreeGroups, Volume::Ext2OneKiB] {
        for pointer in POINTERS {
            let dev = format(volume);
            set_pointer(&dev, 0, pointer, 0);
            assert_refused(dev, &format!("{volume:?} {pointer:?} = 0"), INTO_GROUP_ZERO);
        }
    }
}

/// Block 1 is the superblock on a 1 KiB volume and the descriptor table on
/// a 2 KiB one; block 2 is the descriptor table on the 1 KiB volume.
#[test]
fn a_pointer_onto_the_superblock_or_descriptor_table_is_refused() {
    for (volume, block) in [
        (Volume::Ext4TwoKiBThreeGroups, 1),
        (Volume::Ext2OneKiB, 1),
        (Volume::Ext2OneKiB, 2),
    ] {
        for pointer in POINTERS {
            let dev = format(volume);
            set_pointer(&dev, 0, pointer, block);
            assert_refused(
                dev,
                &format!("{volume:?} {pointer:?} = {block}"),
                INTO_GROUP_ZERO,
            );
        }
    }
}

/// Without `FLEX_BG` a group's bitmaps and inode table live in that group.
/// Group 1's pointers aimed at a free block of group 0, and group 0's aimed
/// into group 1, are both refused.
#[test]
fn without_flex_bg_a_pointer_outside_its_own_group_is_refused() {
    let probe = Filesystem::mount(format(Volume::Ext4TwoKiBThreeGroups)).unwrap();
    let bpg = probe.sb.blocks_per_group;
    let first = probe.sb.first_data_block;
    drop(probe);
    // Past the inode table of group 0 and well clear of any metadata, but
    // also clear of the first two blocks of group 1 in case a pointer is
    // judged as a range.
    let in_group_0 = first + bpg - 2048;
    let in_group_1 = first + bpg + bpg - 2048;
    for (group, target) in [(1u64, in_group_0), (0, in_group_1)] {
        for pointer in POINTERS {
            let dev = format(Volume::Ext4TwoKiBThreeGroups);
            set_pointer(&dev, group, pointer, target);
            assert_refused(
                dev,
                &format!("group {group} {pointer:?} = {target}"),
                OUT_OF_ITS_GROUP,
            );
        }
    }
}
