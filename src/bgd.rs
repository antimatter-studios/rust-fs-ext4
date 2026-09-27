//! Block group descriptor (BGD) parsing.
//!
//! Spec: kernel.org/doc/html/latest/filesystems/ext4/group_descr.html
//!
//! BGDs live in the block(s) immediately following the primary superblock.
//! Each BGD is `superblock.desc_size` bytes (32 legacy, 64 with INCOMPAT_64BIT).

use crate::block_io::BlockDevice;
use crate::checksum::Checksummer;
use crate::error::{Error, Result};
use crate::superblock::Superblock;

/// Block group descriptor (post-parse, all 64-bit fields combined lo+hi).
#[derive(Debug, Clone, Copy)]
pub struct BlockGroupDescriptor {
    pub block_bitmap: u64,
    pub inode_bitmap: u64,
    pub inode_table: u64,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
    pub used_dirs_count: u32,
    pub flags: u16,
    pub itable_unused: u32,
    pub block_bitmap_csum: u32,
    pub inode_bitmap_csum: u32,
    pub checksum: u16,
}

bitflags::bitflags! {
    /// BGD flags (`bg_flags`).
    #[derive(Debug, Clone, Copy)]
    pub struct BgdFlags: u16 {
        /// Inode table not initialized — skip reading (treat as all-free).
        const INODE_UNINIT = 0x0001;
        /// Block bitmap not initialized — treat as all blocks free.
        const BLOCK_UNINIT = 0x0002;
        /// Inode table is fully zeroed on disk.
        const ITABLE_ZEROED = 0x0004;
    }
}

impl BlockGroupDescriptor {
    /// Parse one descriptor from a buffer of at least `desc_size` bytes.
    pub fn parse(buf: &[u8], desc_size: u16) -> Result<Self> {
        if buf.len() < desc_size as usize {
            return Err(Error::Corrupt("bgd buffer too small"));
        }

        let block_bitmap_lo = u32::from_le_bytes(buf[0x00..0x04].try_into().unwrap());
        let inode_bitmap_lo = u32::from_le_bytes(buf[0x04..0x08].try_into().unwrap());
        let inode_table_lo = u32::from_le_bytes(buf[0x08..0x0C].try_into().unwrap());
        let free_blocks_lo = u16::from_le_bytes(buf[0x0C..0x0E].try_into().unwrap());
        let free_inodes_lo = u16::from_le_bytes(buf[0x0E..0x10].try_into().unwrap());
        let used_dirs_lo = u16::from_le_bytes(buf[0x10..0x12].try_into().unwrap());
        let flags = u16::from_le_bytes(buf[0x12..0x14].try_into().unwrap());
        let block_bitmap_csum_lo = u16::from_le_bytes(buf[0x18..0x1A].try_into().unwrap());
        let inode_bitmap_csum_lo = u16::from_le_bytes(buf[0x1A..0x1C].try_into().unwrap());
        let itable_unused_lo = u16::from_le_bytes(buf[0x1C..0x1E].try_into().unwrap());
        let checksum = u16::from_le_bytes(buf[0x1E..0x20].try_into().unwrap());

        let (
            block_bitmap_hi,
            inode_bitmap_hi,
            inode_table_hi,
            free_blocks_hi,
            free_inodes_hi,
            used_dirs_hi,
            itable_unused_hi,
            block_bitmap_csum_hi,
            inode_bitmap_csum_hi,
        ) = if desc_size >= 64 {
            (
                u32::from_le_bytes(buf[0x20..0x24].try_into().unwrap()),
                u32::from_le_bytes(buf[0x24..0x28].try_into().unwrap()),
                u32::from_le_bytes(buf[0x28..0x2C].try_into().unwrap()),
                u16::from_le_bytes(buf[0x2C..0x2E].try_into().unwrap()),
                u16::from_le_bytes(buf[0x2E..0x30].try_into().unwrap()),
                u16::from_le_bytes(buf[0x30..0x32].try_into().unwrap()),
                u16::from_le_bytes(buf[0x32..0x34].try_into().unwrap()),
                u16::from_le_bytes(buf[0x38..0x3A].try_into().unwrap()),
                u16::from_le_bytes(buf[0x3A..0x3C].try_into().unwrap()),
            )
        } else {
            (0, 0, 0, 0, 0, 0, 0, 0, 0)
        };

        Ok(Self {
            block_bitmap: ((block_bitmap_hi as u64) << 32) | block_bitmap_lo as u64,
            inode_bitmap: ((inode_bitmap_hi as u64) << 32) | inode_bitmap_lo as u64,
            inode_table: ((inode_table_hi as u64) << 32) | inode_table_lo as u64,
            free_blocks_count: ((free_blocks_hi as u32) << 16) | free_blocks_lo as u32,
            free_inodes_count: ((free_inodes_hi as u32) << 16) | free_inodes_lo as u32,
            used_dirs_count: ((used_dirs_hi as u32) << 16) | used_dirs_lo as u32,
            flags,
            itable_unused: ((itable_unused_hi as u32) << 16) | itable_unused_lo as u32,
            block_bitmap_csum: ((block_bitmap_csum_hi as u32) << 16) | block_bitmap_csum_lo as u32,
            inode_bitmap_csum: ((inode_bitmap_csum_hi as u32) << 16) | inode_bitmap_csum_lo as u32,
            checksum,
        })
    }

    pub fn flags(&self) -> BgdFlags {
        BgdFlags::from_bits_truncate(self.flags)
    }
}

/// Read all block group descriptors for the filesystem.
///
/// When `csum.enabled`, each descriptor's CRC32C is verified; a mismatch
/// returns `Error::BadChecksum { what: "block group descriptor" }`.
pub fn read_all<D: BlockDevice + ?Sized>(
    dev: &D,
    sb: &Superblock,
    csum: &Checksummer,
) -> Result<Vec<BlockGroupDescriptor>> {
    let block_size = sb.block_size() as u64;

    let group_count = sb.block_group_count();
    // THE TABLE HAS TO BE INSIDE THE FILESYSTEM.
    //
    // `group_count` is `blocks_count / blocks_per_group`, both raw
    // superblock fields, and `desc_size` is another. blocks_count 2^40
    // with blocks_per_group 1 asked for 72 petabytes -- an allocation
    // that aborts through `handle_alloc_error`, past `ffi_guard`'s
    // `catch_unwind`, taking the host process with it. blocks_count
    // u64::MAX wrapped the multiply instead and gave "capacity
    // overflow". Both from a 4 MiB image, at mount. Every descriptor
    // takes its own bytes of the device, wherever the table is laid out.
    let total_bytes = group_count
        .checked_mul(sb.desc_size as u64)
        .ok_or(Error::Corrupt(
            "superblock: the group descriptor table's size overflows",
        ))?;
    if total_bytes > dev.size_bytes() {
        return Err(Error::Corrupt(
            "superblock: the group descriptor table reaches past the end of the device",
        ));
    }

    // Descriptor by descriptor, from wherever each lives: one table after
    // the superblock, or under META_BG a block per meta group (#73). A
    // block is read once for all the descriptors it holds.
    let mut table_block: Option<(u64, Vec<u8>)> = None;
    let mut groups = Vec::with_capacity(group_count as usize);
    for i in 0..(group_count as usize) {
        let (block, off) = sb.descriptor_location(i as u64);
        if table_block.as_ref().map(|(b, _)| *b) != Some(block) {
            let end = block
                .checked_add(1)
                .and_then(|b| b.checked_mul(block_size))
                .ok_or(Error::Corrupt(
                    "superblock: the group descriptor table's extent overflows",
                ))?;
            if end > dev.size_bytes() {
                return Err(Error::Corrupt(
                    "superblock: the group descriptor table reaches past the end of the device",
                ));
            }
            let mut bytes = vec![0u8; block_size as usize];
            dev.read_at(block * block_size, &mut bytes)?;
            table_block = Some((block, bytes));
        }
        let bytes = &table_block.as_ref().unwrap().1;
        let raw = &bytes[off..off + sb.desc_size as usize];
        let stored = u16::from_le_bytes([raw[0x1E], raw[0x1F]]);
        if crate::checksum::group_desc_csum(sb, csum, i as u32, raw)
            .is_some_and(|want| want != stored)
        {
            return Err(Error::BadChecksum {
                what: "block group descriptor",
            });
        }
        let bgd = BlockGroupDescriptor::parse(raw, sb.desc_size)?;
        // THE THREE POINTERS ARE BLOCK NUMBERS IN THIS FILESYSTEM.
        //
        // `bg_block_bitmap`, `bg_inode_bitmap` and `bg_inode_table` are
        // assembled lo|hi into a `u64` and were never checked against
        // anything. Every one of them is a WRITE target:
        // `set_block_run_used` and `free_block_run` write a whole block
        // at the bitmaps, and `locate_inode` hands the table block to
        // `write_inode_raw`, which writes an inode image there. A
        // descriptor pointing outside the filesystem is trivially
        // satisfied on an FSKit mount, where the device is larger than
        // `s_blocks_count`, and `bg_block_bitmap_hi` set high also
        // wrapped the unchecked `bitmap_block * block_size` at the use
        // site. The only comparable check lived in fsck-only code.
        //
        // The inode table spans `inodes_per_group * inode_size` bytes,
        // so it is the one that has to fit as a range rather than a
        // point.
        let table_blocks = (u64::from(sb.inodes_per_group) * u64::from(sb.inode_size))
            .div_ceil(u64::from(sb.block_size()));
        let past_the_end = bgd.block_bitmap >= sb.blocks_count
            || bgd.inode_bitmap >= sb.blocks_count
            || bgd
                .inode_table
                .checked_add(table_blocks)
                .is_none_or(|end| end > sb.blocks_count);
        if past_the_end {
            return Err(Error::Corrupt(
                "a block group descriptor points outside the filesystem",
            ));
        }
        check_pointers_within(sb, i as u64, &bgd, table_blocks)?;
        groups.push(bgd);
    }
    Ok(groups)
}

/// AND BOUNDED BELOW, AND BY THEIR OWN GROUP (#320).
///
/// The bound in [`read_all`] was from above only, so a pointer of 0 -- or 1
/// on a 1 KiB volume -- was accepted, and a bitmap update or inode write then
/// put a whole block over the primary superblock or the descriptor table.
/// This is the rule the kernel's `ext4_check_descriptors` applies:
///
/// - no pointer inside group 0's head: block 0 through the superblock, its
///   descriptor table and the table's reserved growth
///   ([`Superblock::group_head_metadata_blocks`]);
/// - without `FLEX_BG`, every pointer, and the whole inode table, inside the
///   descriptor's own group. With it, `mkfs.ext4` packs a flex group's
///   bitmaps and tables together, so group N's may sit in group 0 and only
///   the filesystem bounds them.
///
/// The caller has already bounded all three by the end of the filesystem.
fn check_pointers_within(
    sb: &Superblock,
    group: u64,
    bgd: &BlockGroupDescriptor,
    table_blocks: u64,
) -> Result<()> {
    // The superblock's own block: `first_data_block` on every volume but a
    // 1 KiB bigalloc one, whose groups start at block 0 while the superblock
    // is still in block 1.
    let superblock_block = crate::superblock::SUPERBLOCK_OFFSET / u64::from(sb.block_size());
    let group0_metadata_end = superblock_block + sb.group_head_metadata_blocks(0);
    let table_last = bgd.inode_table + table_blocks.max(1) - 1;
    if bgd.block_bitmap < group0_metadata_end
        || bgd.inode_bitmap < group0_metadata_end
        || bgd.inode_table < group0_metadata_end
    {
        return Err(Error::Corrupt(
            "a block group descriptor points into group 0's superblock or descriptor table",
        ));
    }
    if sb.feature_incompat & crate::features::Incompat::FLEX_BG.bits() != 0 {
        return Ok(());
    }
    let first = u64::from(sb.first_data_block) + group * u64::from(sb.blocks_per_group);
    let last = first
        .saturating_add(u64::from(sb.blocks_per_group))
        .min(sb.blocks_count)
        - 1;
    let within = |b: u64| (first..=last).contains(&b);
    if !(within(bgd.block_bitmap) && within(bgd.inode_bitmap) && within(bgd.inode_table))
        || table_last > last
    {
        return Err(Error::Corrupt(
            "a block group descriptor points outside its own group, without flex_bg",
        ));
    }
    Ok(())
}

/// Locate the inode table block + offset for a given inode number.
/// Returns (physical block containing the inode, byte offset within block).
pub fn locate_inode(
    sb: &Superblock,
    groups: &[BlockGroupDescriptor],
    ino: u32,
) -> Result<(u64, u32)> {
    if ino == 0 || ino > sb.inodes_count {
        return Err(Error::InvalidInode(ino));
    }
    if sb.inodes_per_group == 0 || sb.inode_size == 0 {
        return Err(Error::Corrupt(
            "superblock: zero inodes_per_group or inode_size",
        ));
    }
    let group_idx = ((ino - 1) / sb.inodes_per_group) as usize;
    let local_idx = ((ino - 1) % sb.inodes_per_group) as u64;

    let bgd = groups.get(group_idx).ok_or(Error::InvalidInode(ino))?;
    let block_size = sb.block_size() as u64;
    let inode_size = sb.inode_size as u64;
    if inode_size > block_size {
        return Err(Error::Corrupt(
            "superblock: inode_size larger than block_size",
        ));
    }
    let inodes_per_block = block_size / inode_size;
    if inodes_per_block == 0 {
        return Err(Error::Corrupt("superblock: inode_size exceeds block_size"));
    }

    let block = bgd
        .inode_table
        .checked_add(local_idx / inodes_per_block)
        .ok_or(Error::Corrupt("inode table block number overflow"))?;
    let off_bytes = (local_idx % inodes_per_block) * inode_size;
    let offset_in_block: u32 = off_bytes
        .try_into()
        .map_err(|_| Error::Corrupt("inode offset exceeds u32"))?;

    Ok((block, offset_in_block))
}
