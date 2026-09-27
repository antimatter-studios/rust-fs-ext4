//! ext4 superblock parsing.
//!
//! Spec: docs/ext4-spec/superblock.md
//! Located at byte offset 1024, 1024 bytes long. Magic 0xEF53 at offset 56.

use crate::block_io::BlockDevice;
use crate::error::{Error, Result};

pub const SUPERBLOCK_OFFSET: u64 = 1024;
pub const SUPERBLOCK_SIZE: usize = 1024;
pub const EXT4_MAGIC: u16 = 0xEF53;

/// `s_state` bits (byte offset 0x3A). The kernel sets `VALID_FS` when a
/// clean unmount completes and clears it on mount; a dirty value on a
/// not-currently-mounted image therefore indicates an unclean shutdown
/// and signals the caller that journal replay (or `fsck`) is required
/// before writes are safe.
pub const EXT4_VALID_FS: u16 = 0x0001;
pub const EXT4_ERROR_FS: u16 = 0x0002;

/// Parsed in-memory representation of the ext4 superblock.
/// Field names mirror the kernel's `struct ext4_super_block` (s_ prefix dropped).
#[derive(Debug, Clone)]
pub struct Superblock {
    pub inodes_count: u32,
    pub blocks_count: u64, // combined lo + hi
    pub free_blocks_count: u64,
    pub free_inodes_count: u32,
    /// `s_r_blocks_count` (lo at 0x08, hi at 0x154 — 64-bit on
    /// INCOMPAT_64BIT volumes). Blocks reserved for the superuser
    /// — explains the gap between `free_blocks` and what `df`
    /// reports as available to a normal user.
    pub r_blocks_count: u64,
    pub first_data_block: u32,
    pub log_block_size: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub magic: u16,
    /// `s_state` (0x3A). `EXT4_VALID_FS` = cleanly unmounted. Any other
    /// value means the FS was mounted and not cleanly unmounted (dirty)
    /// or that the kernel marked the FS as having errors.
    pub state: u16,
    /// `s_errors` (0x3C). Kernel error policy: 1=continue, 2=remount-ro,
    /// 3=panic. Informational from a Swift host's POV but useful in
    /// diagnostics output.
    pub errors_behavior: u16,
    /// `s_minor_rev_level` (0x3E). Bumped by filesystem admin tools for minor format
    /// tweaks within a major rev_level.
    pub minor_rev_level: u16,
    pub rev_level: u32,
    pub inode_size: u16,
    /// `s_first_ino` (0x54, dynamic-rev only). First non-reserved
    /// inode number; defaults to 11 on rev_level=1+ filesystems.
    pub first_inode: u32,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub uuid: [u8; 16],
    pub volume_name: String,
    /// `s_last_mounted` (0x88, 64 bytes). Last directory the FS was
    /// mounted at — handy for diagnostics ("when was this disk last
    /// in another machine?").
    pub last_mounted: String,
    pub desc_size: u16, // BGD size: 32 or 64
    /// `s_reserved_gdt_blocks` — blocks held back after the group
    /// descriptor table so the filesystem can be grown online.
    ///
    /// They sit between the GDT and the block bitmap in every group that
    /// carries a superblock backup, and they are **not** free space:
    /// anything that rebuilds a block bitmap has to mark them used or
    /// the next allocation writes over the filesystem's own room to
    /// expand.
    pub reserved_gdt_blocks: u16,
    /// `s_backup_bgs` — the only two groups carrying backups when
    /// `SPARSE_SUPER2` is set. Meaningless without that feature.
    pub backup_bgs: [u32; 2],
    pub hash_seed: [u32; 4],
    pub default_hash_version: u8,
    pub checksum_seed: u32, // s_checksum_seed (used when INCOMPAT_CSUM_SEED)
    pub journal_inode: u32,
    /// `s_last_orphan` (0xE8). Head of the orphan-inode list — inodes
    /// whose link count reached zero while still open. The kernel
    /// inserts unlink-while-open targets here so that, on the next
    /// mount, recovery can reclaim them. Each inode's `i_dtime` field
    /// is overloaded to point at the next orphan in the chain; the
    /// chain terminates with a zero `dtime`.
    pub last_orphan: u32,
    /// `s_mtime` (0x2C). Timestamp the FS was last mounted.
    pub mtime: u32,
    /// `s_wtime` (0x30). Timestamp the FS was last written to.
    pub wtime: u32,
    /// `s_mnt_count` (0x34). Mounts since last fsck.
    pub mnt_count: u16,
    /// `s_max_mnt_count` (0x36). Forced fsck after this many mounts;
    /// 0 disables.
    pub max_mnt_count: u16,
    /// `s_lastcheck` (0x40). Timestamp of the last fsck pass.
    pub lastcheck: u32,
    /// `s_checkinterval` (0x44). Seconds between forced fscks; 0
    /// disables time-based forced fsck.
    pub checkinterval: u32,
    /// `s_creator_os` (0x48). 0=Linux, 1=Hurd, 2=Masix, 3=FreeBSD,
    /// 4=Lites.
    pub creator_os: u32,
    /// `s_def_resuid` (0x50). UID with access to reserved blocks.
    pub def_resuid: u16,
    /// `s_def_resgid` (0x52). GID with access to reserved blocks.
    pub def_resgid: u16,
    pub raw: Vec<u8>, // keep raw bytes for re-checksum on writes (future)
}

/// The classic `RO_COMPAT_SPARSE_SUPER` layout: groups 0 and 1, and
/// every power of 3, 5 or 7.
///
/// Separate from [`Superblock::group_has_super`] because `mkfs` needs the
/// rule without a filesystem to ask — it is deciding what to write, not
/// reading what someone else wrote.
pub(crate) fn classic_sparse_super(g: u64) -> bool {
    fn is_power_of(mut g: u64, base: u64) -> bool {
        while g.is_multiple_of(base) {
            g /= base;
        }
        g == 1
    }
    g <= 1 || is_power_of(g, 3) || is_power_of(g, 5) || is_power_of(g, 7)
}

/// The largest block ext4 defines, as its base-2 log over 1024.
///
/// 6, which is a 64 KiB block.
pub const MAX_LOG_BLOCK_SIZE: u32 = 6;

/// The first inode a filesystem may hand out, before `s_first_ino`.
///
/// 11. Inodes 1 through 10 are the filesystem's own -- 2 is the root
/// directory and 8 the journal -- and the kernel refuses a superblock
/// whose `s_first_ino` is below it.
pub const GOOD_OLD_FIRST_INODE: u32 = 11;

impl Superblock {
    /// Read and parse the superblock from a block device.
    pub fn read<D: BlockDevice + ?Sized>(dev: &D) -> Result<Self> {
        let mut buf = vec![0u8; SUPERBLOCK_SIZE];
        dev.read_at(SUPERBLOCK_OFFSET, &mut buf)?;
        Self::parse(buf)
    }

    pub fn parse(raw: Vec<u8>) -> Result<Self> {
        if raw.len() < SUPERBLOCK_SIZE {
            return Err(Error::Corrupt("superblock buffer too small"));
        }

        let magic = u16::from_le_bytes([raw[0x38], raw[0x39]]);
        if magic != EXT4_MAGIC {
            return Err(Error::BadMagic {
                found: magic,
                expected: EXT4_MAGIC,
            });
        }

        let inodes_count = u32::from_le_bytes(raw[0x00..0x04].try_into().unwrap());
        let blocks_count_lo = u32::from_le_bytes(raw[0x04..0x08].try_into().unwrap());
        let r_blocks_count_lo = u32::from_le_bytes(raw[0x08..0x0C].try_into().unwrap());
        let free_blocks_count_lo = u32::from_le_bytes(raw[0x0C..0x10].try_into().unwrap());
        let free_inodes_count = u32::from_le_bytes(raw[0x10..0x14].try_into().unwrap());
        let first_data_block = u32::from_le_bytes(raw[0x14..0x18].try_into().unwrap());
        let log_block_size = u32::from_le_bytes(raw[0x18..0x1C].try_into().unwrap());
        let blocks_per_group = u32::from_le_bytes(raw[0x20..0x24].try_into().unwrap());
        let inodes_per_group = u32::from_le_bytes(raw[0x28..0x2C].try_into().unwrap());
        let mtime = u32::from_le_bytes(raw[0x2C..0x30].try_into().unwrap());
        let wtime = u32::from_le_bytes(raw[0x30..0x34].try_into().unwrap());
        let mnt_count = u16::from_le_bytes(raw[0x34..0x36].try_into().unwrap());
        let max_mnt_count = u16::from_le_bytes(raw[0x36..0x38].try_into().unwrap());
        let state = u16::from_le_bytes(raw[0x3A..0x3C].try_into().unwrap());
        let errors_behavior = u16::from_le_bytes(raw[0x3C..0x3E].try_into().unwrap());
        let minor_rev_level = u16::from_le_bytes(raw[0x3E..0x40].try_into().unwrap());
        let lastcheck = u32::from_le_bytes(raw[0x40..0x44].try_into().unwrap());
        let checkinterval = u32::from_le_bytes(raw[0x44..0x48].try_into().unwrap());
        let creator_os = u32::from_le_bytes(raw[0x48..0x4C].try_into().unwrap());
        let rev_level = u32::from_le_bytes(raw[0x4C..0x50].try_into().unwrap());
        let def_resuid = u16::from_le_bytes(raw[0x50..0x52].try_into().unwrap());
        let def_resgid = u16::from_le_bytes(raw[0x52..0x54].try_into().unwrap());

        // Dynamic-rev fields (rev_level >= 1). Pre-rev1 filesystems
        // pin sensible defaults — `inode_size = 128` (the historical
        // ext2 size), `first_inode = 11` (the spec-defined start of
        // user-visible inodes; lower numbers are reserved).
        // A `s_first_ino` below 11 would hand the reserved inodes -- 2
        // is the root directory, 8 the journal -- to the allocator, so
        // the kernel refuses one and this takes the floor rather than
        // the field. Nothing legitimate writes a lower value; a
        // filesystem that does is claiming its own root is available.
        let first_inode = if rev_level >= 1 {
            u32::from_le_bytes(raw[0x54..0x58].try_into().unwrap()).max(GOOD_OLD_FIRST_INODE)
        } else {
            GOOD_OLD_FIRST_INODE
        };
        let inode_size = if rev_level >= 1 {
            u16::from_le_bytes(raw[0x58..0x5A].try_into().unwrap())
        } else {
            128
        };
        let feature_compat = if rev_level >= 1 {
            u32::from_le_bytes(raw[0x5C..0x60].try_into().unwrap())
        } else {
            0
        };
        let feature_incompat = if rev_level >= 1 {
            u32::from_le_bytes(raw[0x60..0x64].try_into().unwrap())
        } else {
            0
        };
        let feature_ro_compat = if rev_level >= 1 {
            u32::from_le_bytes(raw[0x64..0x68].try_into().unwrap())
        } else {
            0
        };

        // s_reserved_gdt_blocks at 0xCE, and s_backup_bgs[2] at 0x24C, between
        // s_overhead_clusters and s_encrypt_algos. Both are zero on a
        // revision-0 filesystem, which has neither.
        let reserved_gdt_blocks = if rev_level >= 1 {
            u16::from_le_bytes(raw[0xCE..0xD0].try_into().unwrap())
        } else {
            0
        };
        let backup_bgs = if rev_level >= 1 && raw.len() >= 0x254 {
            [
                u32::from_le_bytes(raw[0x24C..0x250].try_into().unwrap()),
                u32::from_le_bytes(raw[0x250..0x254].try_into().unwrap()),
            ]
        } else {
            [0, 0]
        };

        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&raw[0x68..0x78]);

        let volume_name_bytes = &raw[0x78..0x88];
        let nul = volume_name_bytes.iter().position(|&b| b == 0).unwrap_or(16);
        let volume_name = String::from_utf8_lossy(&volume_name_bytes[..nul]).into_owned();

        // s_last_mounted at 0x88, 64 bytes. The kernel writes the path
        // here on every successful mount; a freshly mkfs'd filesystem
        // leaves it zero-padded.
        let last_mounted_bytes = &raw[0x88..0xC8];
        let nul = last_mounted_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(64);
        let last_mounted = String::from_utf8_lossy(&last_mounted_bytes[..nul]).into_owned();

        let desc_size = u16::from_le_bytes(raw[0xFE..0x100].try_into().unwrap());
        // If desc_size is 0, default to 32 (legacy); spec says 32 or 64
        let desc_size = if desc_size == 0 { 32 } else { desc_size };

        let mut hash_seed = [0u32; 4];
        for (i, slot) in hash_seed.iter_mut().enumerate() {
            let off = 0xEC + i * 4;
            *slot = u32::from_le_bytes(raw[off..off + 4].try_into().unwrap());
        }
        let default_hash_version = raw[0xFC];

        // 64-bit fields (only valid when INCOMPAT_64BIT). Pre-64bit
        // filesystems leave the high halves zero so combining is safe
        // unconditionally.
        let blocks_count_hi = u32::from_le_bytes(raw[0x150..0x154].try_into().unwrap());
        let r_blocks_count_hi = u32::from_le_bytes(raw[0x154..0x158].try_into().unwrap());
        let free_blocks_count_hi = u32::from_le_bytes(raw[0x158..0x15C].try_into().unwrap());

        let blocks_count = ((blocks_count_hi as u64) << 32) | (blocks_count_lo as u64);
        let r_blocks_count = ((r_blocks_count_hi as u64) << 32) | (r_blocks_count_lo as u64);
        let free_blocks_count =
            ((free_blocks_count_hi as u64) << 32) | (free_blocks_count_lo as u64);

        let checksum_seed = u32::from_le_bytes(raw[0x270..0x274].try_into().unwrap());
        let journal_inode = u32::from_le_bytes(raw[0xE0..0xE4].try_into().unwrap());
        let last_orphan = u32::from_le_bytes(raw[0xE8..0xEC].try_into().unwrap());

        // Reject impossible geometry early so downstream arithmetic never
        // divides by zero. All three are required for the filesystem to
        // name even a single block or inode.
        if blocks_per_group == 0 {
            return Err(Error::Corrupt("superblock: blocks_per_group == 0"));
        }
        if inodes_per_group == 0 {
            return Err(Error::Corrupt("superblock: inodes_per_group == 0"));
        }
        if inode_size == 0 {
            return Err(Error::Corrupt("superblock: inode_size == 0"));
        }
        // ext4 tops out at a 64 KiB block, which is `log_block_size = 6`.
        // The guard used to admit 20, a 1 GiB block, on the argument that
        // anything larger was "certainly a corrupt field" -- which is
        // true of 20 as well. Every `vec![0u8; block_size]` in this
        // crate is sized by this field, and `mount_inner` wraps the
        // device in a 256-entry cache, so a 1 GiB block is 256 GiB of
        // resident memory from a sparse image; three `read_block` calls
        // allocated and zero-filled 3 GiB in five seconds. It is also
        // what makes `ppb * ppb * ppb` overflow in the indirect-block
        // map.
        if log_block_size > MAX_LOG_BLOCK_SIZE {
            return Err(Error::Corrupt(
                "superblock: log_block_size exceeds the largest ext4 block",
            ));
        }
        check_group_fits_bitmap(
            raw.as_slice(),
            log_block_size,
            blocks_per_group,
            feature_ro_compat & crate::features::RoCompat::BIGALLOC.bits() != 0,
        )?;
        // 32 bytes without the 64BIT feature, 64 or more with it, and a
        // power of two either way -- the kernel's own rule. The parser
        // reads fixed offsets up to 0x20, and up to 0x3C when the field
        // says 64, so a smaller value indexes past the buffer: 8 gave
        // "range end index 12 out of range for slice of length 8" during
        // mount.
        let sixty_four_bit = feature_incompat & crate::features::Incompat::BIT64.bits() != 0;
        // WITHOUT 64BIT THE FIELD IS NOT READ AT ALL (#140). The kernel's
        // `ext4_fill_super` sets the descriptor size to 32 on such a volume
        // whatever `s_desc_size` says, and range-checks the field only
        // under 64BIT: a power of two from 64 to 1024. A floor alone let a
        // non-64BIT volume with a padded field of 64 have its descriptors
        // read at 64-byte stride with the 64-bit halves filled from the
        // next descriptor -- and refusing it instead would refuse a volume
        // Linux mounts.
        let desc_size = if sixty_four_bit {
            if !(64..=1024).contains(&desc_size) || !desc_size.is_power_of_two() {
                return Err(Error::Corrupt(
                    "superblock: desc_size is not a group descriptor size",
                ));
            }
            desc_size
        } else {
            32
        };
        if blocks_count == 0 {
            return Err(Error::Corrupt("superblock: blocks_count == 0"));
        }
        // THE FIRST GROUP STARTS INSIDE THE FILESYSTEM (#181). Every block
        // the allocator returns is `group * blocks_per_group +
        // first_data_block + bit`, so a first data block at or past the
        // end put every allocation outside the filesystem -- and on a
        // device larger than it, as an FSKit mount routinely is, inside
        // somebody else's bytes. The kernel refuses both of these in
        // `ext4_fill_super` ("bad geometry").
        if u64::from(first_data_block) >= blocks_count {
            return Err(Error::Corrupt(
                "superblock: first_data_block is at or past the end of the filesystem",
            ));
        }
        // On a 1 KiB block size block 0 is the boot block, so the first
        // group starts at block 1 -- unless clusters are larger than a
        // block, which bigalloc allows.
        if first_data_block == 0
            && log_block_size == 0
            && feature_ro_compat & crate::features::RoCompat::BIGALLOC.bits() == 0
        {
            return Err(Error::Corrupt(
                "superblock: a 1 KiB-block filesystem's first data block is 0, not 1",
            ));
        }

        Ok(Self {
            inodes_count,
            blocks_count,
            free_blocks_count,
            free_inodes_count,
            r_blocks_count,
            first_data_block,
            log_block_size,
            blocks_per_group,
            inodes_per_group,
            magic,
            state,
            errors_behavior,
            minor_rev_level,
            rev_level,
            inode_size,
            first_inode,
            feature_compat,
            feature_incompat,
            feature_ro_compat,
            uuid,
            volume_name,
            last_mounted,
            desc_size,
            reserved_gdt_blocks,
            backup_bgs,
            hash_seed,
            default_hash_version,
            checksum_seed,
            journal_inode,
            last_orphan,
            mtime,
            wtime,
            mnt_count,
            max_mnt_count,
            lastcheck,
            checkinterval,
            creator_os,
            def_resuid,
            def_resgid,
            raw,
        })
    }

    /// Whether the filesystem was cleanly unmounted. `false` here means
    /// the FS was not cleanly unmounted and a journal replay (or fsck)
    /// is required before writes are safe. Read-only consumers can
    /// still mount a dirty FS; callers that intend to write should
    /// surface this to the user and either run fsck or refuse to
    /// mount read-write.
    pub fn is_clean(&self) -> bool {
        self.state & EXT4_VALID_FS != 0
    }

    /// Whether block group `g` carries a superblock and group-descriptor
    /// backup.
    ///
    /// Three layouts, and the filesystem's own flags decide which:
    ///
    /// - **`SPARSE_SUPER2`**: group 0, and the two groups named by
    ///   `s_backup_bgs`. Nothing else.
    /// - **`SPARSE_SUPER` clear**: every group. This is the old ext2
    ///   layout, and the one that costs most to get wrong — assuming the
    ///   sparse rule leaves real backups looking like free space.
    /// - **otherwise**: the classic sparse rule — groups 0 and 1, and
    ///   every power of 3, 5 or 7.
    ///
    /// Answering unconditionally with the classic rule is wrong in both
    /// of the first two cases, and wrong in the direction that matters:
    /// on a filesystem without `SPARSE_SUPER` it reports "no backup here"
    /// for groups that have one, so a rebuilt bitmap offers the backup
    /// superblock and its descriptor table as free blocks.
    pub fn group_has_super(&self, g: u64) -> bool {
        use crate::features::{Compat, RoCompat};

        if g == 0 {
            return true;
        }
        if self.feature_compat & Compat::SPARSE_SUPER2.bits() != 0 {
            return self.backup_bgs.iter().any(|&b| u64::from(b) == g);
        }
        if self.feature_ro_compat & RoCompat::SPARSE_SUPER.bits() == 0 {
            return true;
        }
        classic_sparse_super(g)
    }

    /// Group descriptors per descriptor-table block.
    pub fn descs_per_block(&self) -> u64 {
        u64::from(self.block_size()) / u64::from(self.desc_size.max(1))
    }

    /// `s_first_meta_bg` (0x104): the first meta group whose descriptors
    /// live in the meta group rather than the contiguous table. Zero
    /// without `META_BG`.
    pub fn first_meta_bg(&self) -> u64 {
        if self.feature_incompat & crate::features::Incompat::META_BG.bits() == 0 {
            return 0;
        }
        self.raw
            .get(0x104..0x108)
            .map_or(0, |f| u64::from(u32::from_le_bytes(f.try_into().unwrap())))
    }

    fn in_meta_bg(&self, group: u64) -> bool {
        self.feature_incompat & crate::features::Incompat::META_BG.bits() != 0
            && group / self.descs_per_block() >= self.first_meta_bg()
    }

    /// Where group `group`'s descriptor lives: the block, and the byte
    /// offset within it. The kernel's `descriptor_loc` (#73).
    ///
    /// Without `META_BG` -- or for a meta group before `s_first_meta_bg` --
    /// the descriptors are one table starting the block after the
    /// superblock. With it, meta group `m` (the `descs_per_block` groups
    /// from `m * descs_per_block`) keeps its one descriptor block at the head
    /// of its own first group, after that group's superblock backup if it has
    /// one. The copies in the meta group's second and last groups are
    /// backups.
    pub fn descriptor_location(&self, group: u64) -> (u64, usize) {
        let dpb = self.descs_per_block();
        let metagroup = group / dpb;
        let offset = ((group % dpb) * u64::from(self.desc_size)) as usize;
        if !self.in_meta_bg(group) {
            // The table follows the superblock's own block, which is
            // `first_data_block` on every volume but one: a 1 KiB bigalloc
            // volume, whose groups start at block 0 while the superblock is
            // still in block 1 (#75).
            return (
                SUPERBLOCK_OFFSET / u64::from(self.block_size()) + 1 + metagroup,
                offset,
            );
        }
        let first = metagroup * dpb;
        let mut has_super = u64::from(self.group_has_super(first));
        // A 1 KiB filesystem whose groups start at block 0 has the primary
        // superblock in block 1.
        if self.block_size() == 1024 && metagroup == 0 && self.first_data_block == 0 {
            has_super += 1;
        }
        let group_start =
            u64::from(self.first_data_block) + first * u64::from(self.blocks_per_group);
        (group_start + has_super, offset)
    }

    /// Blocks at the head of `group` that belong to the superblock backup,
    /// the descriptor table and its reserved growth: the kernel's
    /// `ext4_num_base_meta_clusters` less the bitmaps and inode table.
    pub fn group_head_metadata_blocks(&self, group: u64) -> u64 {
        let has_super = u64::from(self.group_has_super(group));
        if self.in_meta_bg(group) {
            let dpb = self.descs_per_block();
            let first = (group / dpb) * dpb;
            let gdt = u64::from(group == first || group == first + 1 || group == first + dpb - 1);
            return has_super + gdt;
        }
        if has_super == 0 {
            return 0;
        }
        let gdt = if self.feature_incompat & crate::features::Incompat::META_BG.bits() != 0 {
            self.first_meta_bg()
        } else {
            (self.block_group_count() * u64::from(self.desc_size))
                .div_ceil(u64::from(self.block_size()))
        };
        1 + gdt + u64::from(self.reserved_gdt_blocks)
    }

    /// Whether directory names are hashed as unsigned bytes: `s_flags`
    /// (0x160) carries `EXT2_FLAGS_UNSIGNED_HASH` (0x2). See
    /// [`crate::hash::effective_version`].
    pub fn unsigned_hash(&self) -> bool {
        self.raw
            .get(0x160..0x164)
            .is_some_and(|f| u32::from_le_bytes(f.try_into().unwrap()) & 0x2 != 0)
    }

    /// Block size in bytes: 1024 << log_block_size.
    pub fn block_size(&self) -> u32 {
        1024u32 << self.log_block_size
    }

    /// `blocks_count * block_size`, or `None` when the product overflows
    /// a `u64` -- which no real filesystem does, so a caller refuses it
    /// rather than saturating to a bound that admits anything.
    pub fn filesystem_bytes(&self) -> Option<u64> {
        self.blocks_count.checked_mul(u64::from(self.block_size()))
    }

    /// THE FILESYSTEM FITS ITS DEVICE (#321), the kernel's "block count
    /// exceeds size of device". Every bound that reads `blocks_count`
    /// otherwise trusts a size nothing checked: a superblock claiming
    /// 2^32 blocks on a few megabytes let `read_all` ask for terabytes.
    /// A device larger than the filesystem is normal (a partition, an
    /// FSKit volume); a smaller one is a truncated or forged image.
    pub fn check_fits_device(&self, device_bytes: u64) -> Result<()> {
        match self.filesystem_bytes() {
            Some(bytes) if bytes <= device_bytes => Ok(()),
            _ => Err(Error::Corrupt(
                "superblock: blocks_count makes the filesystem larger than the device",
            )),
        }
    }

    /// Number of block groups.
    ///
    /// Counted from `s_first_data_block`, as the kernel's `ext4_fill_super`
    /// does: on a 1 KiB-block filesystem block 0 is the boot block and the
    /// groups start at block 1. Dividing the whole of `s_blocks_count`
    /// gave one group too many whenever it was one past a multiple of
    /// `s_blocks_per_group`, and the phantom group's descriptor -- zeros --
    /// failed its checksum and refused a volume Linux mounts (#86). The
    /// rest of the crate (`alloc::blocks_in_group`, the block-to-group
    /// mapping) already subtracted it.
    pub fn block_group_count(&self) -> u64 {
        group_count(
            self.blocks_count,
            self.first_data_block,
            self.blocks_per_group,
        )
    }

    /// Whether the 64BIT incompat feature is enabled.
    pub fn is_64bit(&self) -> bool {
        self.feature_incompat & crate::features::Incompat::BIT64.bits() != 0
    }
}

#[cfg(test)]
mod backup_layout_tests {
    use super::*;
    use crate::features::{Compat, RoCompat};

    /// A superblock with only the fields the backup-layout rule reads.
    fn sb_with(compat: u32, ro_compat: u32, backup_bgs: [u32; 2], reserved_gdt: u16) -> Superblock {
        let mut raw = vec![0u8; SUPERBLOCK_SIZE];
        raw[0x38..0x3A].copy_from_slice(&EXT4_MAGIC.to_le_bytes());
        raw[0x00..0x04].copy_from_slice(&8192u32.to_le_bytes()); // inodes_count
        raw[0x04..0x08].copy_from_slice(&65536u32.to_le_bytes()); // blocks_count
        raw[0x14..0x18].copy_from_slice(&1u32.to_le_bytes()); // first_data_block
        raw[0x20..0x24].copy_from_slice(&8192u32.to_le_bytes()); // blocks_per_group
        raw[0x28..0x2C].copy_from_slice(&2048u32.to_le_bytes()); // inodes_per_group
        raw[0x4C..0x50].copy_from_slice(&1u32.to_le_bytes()); // rev_level
        raw[0x58..0x5A].copy_from_slice(&256u16.to_le_bytes()); // inode_size
        raw[0x5C..0x60].copy_from_slice(&compat.to_le_bytes());
        raw[0x64..0x68].copy_from_slice(&ro_compat.to_le_bytes());
        raw[0xCE..0xD0].copy_from_slice(&reserved_gdt.to_le_bytes());
        raw[0xFE..0x100].copy_from_slice(&64u16.to_le_bytes()); // desc_size
        raw[0x24C..0x250].copy_from_slice(&backup_bgs[0].to_le_bytes());
        raw[0x250..0x254].copy_from_slice(&backup_bgs[1].to_le_bytes());
        Superblock::parse(raw).expect("superblock")
    }

    /// The layout nearly every filesystem has: groups 0 and 1, then the
    /// powers of 3, 5 and 7.
    #[test]
    fn the_classic_sparse_layout() {
        let sb = sb_with(0, RoCompat::SPARSE_SUPER.bits(), [0, 0], 0);
        for g in [0, 1, 3, 5, 7, 9, 25, 27, 49, 81, 125] {
            assert!(sb.group_has_super(g), "group {g} should carry a backup");
        }
        for g in [2, 4, 6, 8, 10, 11, 26, 50, 100] {
            assert!(!sb.group_has_super(g), "group {g} should not");
        }
    }

    /// Without `SPARSE_SUPER`, every group carries one.
    ///
    /// This is the case that costs data rather than capacity: answering
    /// with the sparse rule reports "no backup here" for groups that have
    /// one, so rebuilding a bitmap offers a live backup superblock and its
    /// descriptor table as free space.
    #[test]
    fn without_sparse_super_every_group_carries_a_backup() {
        let sb = sb_with(0, 0, [0, 0], 0);
        for g in 0..40 {
            assert!(sb.group_has_super(g), "group {g} should carry a backup");
        }
    }

    /// `SPARSE_SUPER2` names exactly two groups, and no rule applies
    /// beyond them.
    #[test]
    fn sparse_super2_names_its_own_groups() {
        let sb = sb_with(
            Compat::SPARSE_SUPER2.bits(),
            RoCompat::SPARSE_SUPER.bits(),
            [4, 17],
            0,
        );
        assert!(sb.group_has_super(0), "group 0 always carries one");
        assert!(sb.group_has_super(4));
        assert!(sb.group_has_super(17));
        // Powers of 3, 5 and 7 are not special here, and group 1 is not
        // either — which is what distinguishes this from the classic rule
        // rather than merely narrowing it.
        for g in [1, 3, 5, 7, 9, 25, 49] {
            assert!(
                !sb.group_has_super(g),
                "group {g} is not named by s_backup_bgs and must not be treated as \
                 carrying a backup"
            );
        }
    }

    /// The field that says how much room the filesystem keeps to grow.
    #[test]
    fn reserved_gdt_blocks_is_read() {
        assert_eq!(
            sb_with(0, RoCompat::SPARSE_SUPER.bits(), [0, 0], 1024).reserved_gdt_blocks,
            1024
        );
        assert_eq!(
            sb_with(0, RoCompat::SPARSE_SUPER.bits(), [0, 0], 0).reserved_gdt_blocks,
            0
        );
    }
}

#[cfg(test)]
mod geometry_tests {
    use super::*;

    fn raw(blocks_count: u32, first_data_block: u32, log_block_size: u32) -> Vec<u8> {
        let mut raw = vec![0u8; SUPERBLOCK_SIZE];
        raw[0x38..0x3A].copy_from_slice(&EXT4_MAGIC.to_le_bytes());
        raw[0x00..0x04].copy_from_slice(&8192u32.to_le_bytes()); // inodes_count
        raw[0x04..0x08].copy_from_slice(&blocks_count.to_le_bytes());
        raw[0x14..0x18].copy_from_slice(&first_data_block.to_le_bytes());
        raw[0x18..0x1C].copy_from_slice(&log_block_size.to_le_bytes());
        raw[0x20..0x24].copy_from_slice(&8192u32.to_le_bytes()); // blocks_per_group
        raw[0x28..0x2C].copy_from_slice(&2048u32.to_le_bytes()); // inodes_per_group
        raw[0x4C..0x50].copy_from_slice(&1u32.to_le_bytes()); // rev_level
        raw[0x58..0x5A].copy_from_slice(&256u16.to_le_bytes()); // inode_size
        raw[0xFE..0x100].copy_from_slice(&64u16.to_le_bytes()); // desc_size
        raw
    }

    fn refusal(raw: Vec<u8>) -> String {
        match Superblock::parse(raw) {
            Err(Error::Corrupt(m)) => m.to_string(),
            other => panic!("expected Corrupt, got {:?}", other.map(|_| ())),
        }
    }

    /// A first data block at or past the end is refused, as the kernel's
    /// "bad geometry" check does (#181).
    #[test]
    fn a_first_data_block_past_the_end_is_refused() {
        for (blocks, first) in [(16384u32, 16384u32), (16384, 20000), (100, u32::MAX)] {
            assert!(
                refusal(raw(blocks, first, 0)).contains("first_data_block"),
                "blocks {blocks}, first {first}"
            );
        }
    }

    /// And a 1 KiB-block filesystem that starts its groups at block 0 --
    /// unless it is bigalloc, whose clusters are larger than a block and
    /// may start at 0.
    #[test]
    fn a_one_kib_filesystem_starting_at_block_zero_is_refused() {
        assert!(refusal(raw(16384, 0, 0)).contains("first data block is 0"));

        let mut bigalloc = raw(16384, 0, 0);
        bigalloc[0x64..0x68]
            .copy_from_slice(&crate::features::RoCompat::BIGALLOC.bits().to_le_bytes());
        bigalloc[0x24..0x28].copy_from_slice(&8192u32.to_le_bytes()); // clusters_per_group
        Superblock::parse(bigalloc)
            .expect("a 1 KiB bigalloc filesystem may start its groups at block 0");
    }

    /// A group has one bitmap block, so it cannot have more blocks than
    /// that block has bits -- the kernel's "blocks per group too big"
    /// (#321). Without this bound the allocator's scan ran past the
    /// bitmap into the next group's blocks.
    #[test]
    fn more_blocks_per_group_than_a_bitmap_has_bits_is_refused() {
        for (log, bpg) in [(0u32, 8193u32), (2, 32769), (2, u32::MAX)] {
            let mut r = raw(65536, u32::from(log == 0), log);
            r[0x20..0x24].copy_from_slice(&bpg.to_le_bytes());
            assert!(
                refusal(r).contains("blocks_per_group"),
                "log {log}, blocks_per_group {bpg}"
            );
        }
        for (log, bpg) in [(0u32, 8192u32), (2, 32768)] {
            let mut r = raw(65536, u32::from(log == 0), log);
            r[0x20..0x24].copy_from_slice(&bpg.to_le_bytes());
            Superblock::parse(r)
                .unwrap_or_else(|e| panic!("log {log}, blocks_per_group {bpg}: {e:?}"));
        }
    }

    /// Under bigalloc the bitmap counts clusters, so the bound is on
    /// `s_clusters_per_group`, and `s_blocks_per_group` must be that
    /// many clusters in blocks -- the kernel's two bigalloc checks.
    #[test]
    fn a_bigalloc_group_is_bounded_in_clusters() {
        let bigalloc = |log_cluster: u32, cpg: u32, bpg: u32| {
            let mut r = raw(1 << 20, 0, 2);
            r[0x64..0x68]
                .copy_from_slice(&crate::features::RoCompat::BIGALLOC.bits().to_le_bytes());
            r[0x1C..0x20].copy_from_slice(&log_cluster.to_le_bytes());
            r[0x20..0x24].copy_from_slice(&bpg.to_le_bytes());
            r[0x24..0x28].copy_from_slice(&cpg.to_le_bytes());
            r
        };
        // 64 KiB clusters over 4 KiB blocks: 32768 clusters is 16 * 32768
        // blocks, far more than a 4 KiB bitmap has bits, and legitimate.
        Superblock::parse(bigalloc(6, 32768, 32768 * 16)).expect("mke2fs -O bigalloc -C 65536");
        assert!(refusal(bigalloc(6, 32769, 32769 * 16)).contains("clusters_per_group"));
        assert!(refusal(bigalloc(6, 32768, 32768)).contains("clusters_per_group"));
        assert!(refusal(bigalloc(6, 0, 32768)).contains("clusters_per_group"));
        assert!(refusal(bigalloc(1, 32768, 32768)).contains("cluster"));
        assert!(refusal(bigalloc(40, 1, 1)).contains("cluster"));
    }

    /// The geometries mke2fs writes still parse: 1 KiB from block 1, and
    /// larger blocks from block 0.
    #[test]
    fn the_geometries_mke2fs_writes_still_parse() {
        for (blocks, first, log) in [
            (16385u32, 1u32, 0u32),
            (16384, 1, 0),
            (65536, 0, 2),
            (1, 0, 2),
        ] {
            Superblock::parse(raw(blocks, first, log))
                .unwrap_or_else(|e| panic!("blocks {blocks}, first {first}, log {log}: {e:?}"));
        }
    }
}

/// The kernel's largest `s_log_cluster_size`: `EXT4_MAX_CLUSTER_LOG_SIZE`
/// (30) less `EXT4_MIN_BLOCK_LOG_SIZE` (10), a 1 GiB cluster.
const MAX_LOG_CLUSTER_SIZE: u32 = 20;

/// A GROUP FITS ITS BITMAP (#321). Each group has one bitmap block, so it
/// cannot track more units than that block has bits. Without this bound
/// the allocator's scan ran past the bitmap -- `bit_is_set` reads a bit
/// past the end as free -- and handed out the next group's blocks, and a
/// single group of `u32::MAX` blocks passed the descriptor-table check.
///
/// The unit is a block, or under BIGALLOC a cluster: there the bitmap
/// counts clusters, `s_clusters_per_group` is what is bounded, and
/// `s_blocks_per_group` must be exactly that many clusters in blocks.
/// These are `ext4_handle_clustersize`'s checks.
fn check_group_fits_bitmap(
    raw: &[u8],
    log_block_size: u32,
    blocks_per_group: u32,
    bigalloc: bool,
) -> Result<()> {
    let bitmap_bits = 8u64 << (10 + log_block_size);
    if !bigalloc {
        if u64::from(blocks_per_group) > bitmap_bits {
            return Err(Error::Corrupt(
                "superblock: blocks_per_group exceeds the bits in one bitmap block",
            ));
        }
        return Ok(());
    }
    let log_cluster_size = u32::from_le_bytes(raw[0x1C..0x20].try_into().unwrap());
    if log_cluster_size < log_block_size || log_cluster_size > MAX_LOG_CLUSTER_SIZE {
        return Err(Error::Corrupt(
            "superblock: bigalloc cluster is smaller than a block or larger than ext4 allows",
        ));
    }
    let clusters_per_group = u32::from_le_bytes(raw[0x24..0x28].try_into().unwrap());
    if clusters_per_group == 0 || u64::from(clusters_per_group) > bitmap_bits {
        return Err(Error::Corrupt(
            "superblock: clusters_per_group is zero or exceeds the bits in one bitmap block",
        ));
    }
    let cluster_ratio_log = log_cluster_size - log_block_size;
    if u64::from(clusters_per_group) << cluster_ratio_log != u64::from(blocks_per_group) {
        return Err(Error::Corrupt(
            "superblock: blocks_per_group is not clusters_per_group clusters",
        ));
    }
    Ok(())
}

/// `ceil((blocks_count - first_data_block) / blocks_per_group)`, the
/// kernel's group count; see [`Superblock::block_group_count`].
pub fn group_count(blocks_count: u64, first_data_block: u32, blocks_per_group: u32) -> u64 {
    blocks_count
        .saturating_sub(u64::from(first_data_block))
        .div_ceil(u64::from(blocks_per_group).max(1))
}

#[cfg(test)]
mod group_count_tests {
    use super::group_count;

    /// Against the kernel's arithmetic, including the 1 KiB geometries
    /// one block past a multiple of the group size, where dividing the
    /// whole block count gave one group too many (#86).
    #[test]
    fn the_group_count_starts_at_the_first_data_block() {
        for (blocks, first, bpg, groups) in [
            (16385u64, 1u32, 8192u32, 2u64),
            (16384, 1, 8192, 2),
            (24577, 1, 8192, 3),
            (8193, 1, 8192, 1),
            (16385, 0, 8192, 3),
            (32768, 0, 32768, 1),
            (32769, 0, 32768, 2),
        ] {
            assert_eq!(
                group_count(blocks, first, bpg),
                groups,
                "blocks={blocks} first_data_block={first} blocks_per_group={bpg}"
            );
        }
    }
}

#[cfg(test)]
mod desc_size_tests {
    use super::*;

    fn raw(desc_size: u16, sixty_four_bit: bool) -> Vec<u8> {
        let mut raw = vec![0u8; SUPERBLOCK_SIZE];
        raw[0x38..0x3A].copy_from_slice(&EXT4_MAGIC.to_le_bytes());
        raw[0x00..0x04].copy_from_slice(&8192u32.to_le_bytes()); // inodes_count
        raw[0x04..0x08].copy_from_slice(&65536u32.to_le_bytes()); // blocks_count
        raw[0x18..0x1C].copy_from_slice(&2u32.to_le_bytes()); // 4 KiB blocks
        raw[0x20..0x24].copy_from_slice(&32768u32.to_le_bytes()); // blocks_per_group
        raw[0x28..0x2C].copy_from_slice(&4096u32.to_le_bytes()); // inodes_per_group
        raw[0x4C..0x50].copy_from_slice(&1u32.to_le_bytes()); // rev_level
        raw[0x58..0x5A].copy_from_slice(&256u16.to_le_bytes()); // inode_size
        if sixty_four_bit {
            let incompat = crate::features::Incompat::BIT64.bits();
            raw[0x60..0x64].copy_from_slice(&incompat.to_le_bytes());
        }
        raw[0xFE..0x100].copy_from_slice(&desc_size.to_le_bytes());
        raw
    }

    /// Without 64BIT the descriptor is 32 bytes whatever the field says,
    /// as the kernel reads it (#140).
    #[test]
    fn without_64bit_the_descriptor_is_32_bytes_whatever_the_field_says() {
        for field in [0u16, 32, 64, 128, 1024, 33] {
            let sb = Superblock::parse(raw(field, false))
                .unwrap_or_else(|e| panic!("field {field}: {e:?}"));
            assert_eq!(sb.desc_size, 32, "field {field}");
        }
    }

    /// With 64BIT the field is the size, a power of two from 64 to 1024.
    #[test]
    fn with_64bit_the_field_is_range_checked() {
        for field in [64u16, 128, 1024] {
            assert_eq!(
                Superblock::parse(raw(field, true)).unwrap().desc_size,
                field
            );
        }
        for field in [0u16, 32, 96, 2048] {
            assert!(
                matches!(Superblock::parse(raw(field, true)), Err(Error::Corrupt(_))),
                "field {field} must be refused"
            );
        }
    }
}
