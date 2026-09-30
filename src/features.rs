//! ext4 feature flags — COMPAT, INCOMPAT, RO_COMPAT.
//!
//! Spec source: kernel.org/doc/html/latest/filesystems/ext4/super.html

use bitflags::bitflags;

bitflags! {
    /// COMPAT features — safe to ignore if unknown.
    #[derive(Debug, Clone, Copy)]
    pub struct Compat: u32 {
        const DIR_PREALLOC      = 0x0001;
        const IMAGIC_INODES     = 0x0002;
        const HAS_JOURNAL       = 0x0004;
        const EXT_ATTR          = 0x0008;
        const RESIZE_INODE      = 0x0010;
        const DIR_INDEX         = 0x0020;  // HTree
        const LAZY_BG           = 0x0040;
        const SPARSE_SUPER2     = 0x0200;
        const FAST_COMMIT       = 0x0400;
        const ORPHAN_FILE       = 0x1000;
    }

    /// INCOMPAT features — kernel MUST understand or refuse to mount.
    #[derive(Debug, Clone, Copy)]
    pub struct Incompat: u32 {
        const COMPRESSION       = 0x00001;
        const FILETYPE          = 0x00002;
        const RECOVER           = 0x00004;
        const JOURNAL_DEV       = 0x00008;
        const META_BG           = 0x00010;
        const EXTENTS           = 0x00040;
        const BIT64             = 0x00080;
        const MMP               = 0x00100;
        const FLEX_BG           = 0x00200;
        const EA_INODE          = 0x00400;
        const DIRDATA           = 0x01000;
        const CSUM_SEED         = 0x02000;
        const LARGEDIR          = 0x04000;
        const INLINE_DATA       = 0x08000;
        const ENCRYPT           = 0x10000;
        const CASEFOLD          = 0x20000;
    }

    /// RO_COMPAT features — must mount read-only if unknown.
    #[derive(Debug, Clone, Copy)]
    pub struct RoCompat: u32 {
        const SPARSE_SUPER      = 0x0001;
        const LARGE_FILE        = 0x0002;
        const BTREE_DIR         = 0x0004;
        const HUGE_FILE         = 0x0008;
        const GDT_CSUM          = 0x0010;
        const DIR_NLINK         = 0x0020;
        const EXTRA_ISIZE       = 0x0040;
        const HAS_SNAPSHOT      = 0x0080;
        const QUOTA             = 0x0100;
        const BIGALLOC          = 0x0200;
        const METADATA_CSUM     = 0x0400;
        const REPLICA           = 0x0800;
        const READONLY          = 0x1000;
        const PROJECT           = 0x2000;
        const VERITY            = 0x8000;
        const ORPHAN_PRESENT    = 0x10000;
    }
}

/// INCOMPAT bits this driver mounts. Anything else in feature_incompat
/// means refuse-to-mount. Some of these can be read and not written: see
/// [`WRITE_BREAKING_INCOMPAT`].
pub const SUPPORTED_INCOMPAT: u32 = Incompat::FILETYPE.bits()
    | Incompat::EXTENTS.bits()
    | Incompat::BIT64.bits()
    | Incompat::FLEX_BG.bits()
    | Incompat::CSUM_SEED.bits()
    // Descriptors kept per meta group rather than in one table: found
    // through `Superblock::descriptor_location` (#73).
    | Incompat::META_BG.bits()
    // "Some inode may be encrypted", not "the volume is": fscrypt is a
    // per-directory policy and everything outside it is plain. Encrypted
    // contents and names are refused per inode (`file_io::refuse_encrypted`,
    // #76); writes are refused volume-wide, see WRITE_BREAKING_INCOMPAT.
    | Incompat::ENCRYPT.bits()
    // A dirty journal: replayed onto the device by a writable mount
    // (`journal_apply::replay_if_dirty`), and into the buffer cache by a
    // read-only one (`journal_apply::replay_into_cache`, #72).
    | Incompat::RECOVER.bits()
    // Read-only only: see WRITE_BREAKING_INCOMPAT.
    | Incompat::MMP.bits()
    // Inline files are read in full, `i_block` then the `system.data`
    // xattr (`inline_data::read_all`). An inline directory is searched in
    // `i_block` only: see G3 in docs/format-conformance-gaps.md.
    | Incompat::INLINE_DATA.bits()
    | Incompat::LARGEDIR.bits()
    | Incompat::EA_INODE.bits()
    | Incompat::CASEFOLD.bits();

/// INCOMPAT bits that are safe to READ and not safe to WRITE.
///
/// These are the mirror of [`MAINTAINED_RO_COMPAT`] on the incompat side.
/// The RO_COMPAT rule — a reader that does not know the bit may read and
/// must not write — has no INCOMPAT equivalent in the format itself, but
/// some INCOMPAT features have exactly that shape in practice: this
/// driver can locate every byte on such a volume, and cannot keep the
/// structures the bit describes correct once it starts writing.
///
/// - `MMP` (multi-mount protection) stops two hosts mounting one
///   filesystem read-write at once. Honouring it means reading the MMP
///   block, checking its sequence, writing our own node name, waiting and
///   re-checking; none of that exists. A reader cannot corrupt anything
///   and does not affect the other host's protection, so read-only is
///   fine and writing is not.
///
/// - `CASEFOLD` changes how a directory entry's htree slot is CHOSEN: the
///   directory's htree hash is computed over the normalised, case-folded
///   name (`casefold::casefold_name_hash`), and the entry is filed into
///   the leaf that hash selects. This driver's write path hashes the raw
///   bytes. Reads survive that by accident,
///   because `path::find_entry` falls back to a linear scan when the
///   htree descent misses — slower, and case-insensitive lookup does not
///   work, but nothing is lost. A write does not survive it: the entry
///   goes into the leaf the wrong hash names, this driver's linear scan
///   keeps finding it, and the kernel's htree descent does not. The file
///   becomes invisible on Linux while still holding its inode and its
///   directory slot, and `e2fsck` reports the htree as inconsistent.
///
/// `casefold.rs` implements the hash and has no callers; neither
/// `s_encoding` nor `EXT4_CASEFOLD_FL` is read anywhere, so the driver
/// cannot currently tell that a directory is casefolded at all. Wiring
/// that up is what would let this bit move out of here.
///
/// - `ENCRYPT` is read per inode, and a write would have to be refused per
///   inode too: a name created in an encrypted directory, or data written
///   to an encrypted file, is plaintext where the kernel reads ciphertext.
///   Nothing on the write side checks `EXT4_ENCRYPT_FL`, so the volume is
///   read-only here (#76).
pub const WRITE_BREAKING_INCOMPAT: u32 =
    Incompat::MMP.bits() | Incompat::CASEFOLD.bits() | Incompat::ENCRYPT.bits();

/// The INCOMPAT bits on this volume that a write here would not keep
/// consistent. Zero means the volume may be mounted read-write.
pub fn write_breaking_incompat(feature_incompat: u32) -> u32 {
    feature_incompat & WRITE_BREAKING_INCOMPAT
}

/// RO_COMPAT bits a mount accepts. Accepting one is not the same as keeping
/// it correct through a write: see [`MAINTAINED_RO_COMPAT`].
pub const SUPPORTED_RO_COMPAT: u32 = RoCompat::SPARSE_SUPER.bits()
    | RoCompat::LARGE_FILE.bits()
    | RoCompat::HUGE_FILE.bits()
    | RoCompat::GDT_CSUM.bits()
    | RoCompat::DIR_NLINK.bits()
    | RoCompat::EXTRA_ISIZE.bits()
    | RoCompat::QUOTA.bits()
    | RoCompat::METADATA_CSUM.bits()
    | RoCompat::PROJECT.bits()
    | RoCompat::ORPHAN_PRESENT.bits()
    // Read like any volume; not maintained, so not written (#75).
    | RoCompat::BIGALLOC.bits();

/// Filesystem dialect — derived from the on-disk feature flags at mount time.
/// Drives runtime behaviour where ext2 / ext3 / ext4 differ:
///
/// - inode block-mapping scheme (extent tree vs legacy direct/indirect)
/// - presence of a journal (replay path runs only for `Ext3`/`Ext4`)
/// - which features new inodes opt into when the driver creates them
///
/// The classification mirrors what the Linux kernel's single `ext4` driver
/// uses internally — there is no separate ext2 driver in this crate, just
/// runtime dispatch keyed on this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsFlavor {
    /// No EXTENTS, no HAS_JOURNAL.
    Ext2,
    /// No EXTENTS, HAS_JOURNAL set (jbd2-style log on a hidden journal inode).
    Ext3,
    /// EXTENTS set (with or without a journal — modern ext4 typically has one).
    Ext4,
}

impl FsFlavor {
    /// Derive flavor from the parsed superblock's COMPAT/INCOMPAT bits.
    pub fn detect(feature_compat: u32, feature_incompat: u32) -> Self {
        let has_extents = (feature_incompat & Incompat::EXTENTS.bits()) != 0;
        let has_journal = (feature_compat & Compat::HAS_JOURNAL.bits()) != 0;
        match (has_extents, has_journal) {
            (true, _) => FsFlavor::Ext4,
            (false, true) => FsFlavor::Ext3,
            (false, false) => FsFlavor::Ext2,
        }
    }

    /// True when the driver should allocate new inodes with `EXT4_EXTENTS_FL`
    /// set (so file contents are tracked by an extent tree). False for ext2/3,
    /// which use the legacy direct/indirect block-pointer scheme.
    pub fn uses_extents(&self) -> bool {
        matches!(self, FsFlavor::Ext4)
    }

    /// True when this volume has a journal that must be replayed (or honoured
    /// on writes). Ext2 has none.
    pub fn has_journal(&self) -> bool {
        matches!(self, FsFlavor::Ext3 | FsFlavor::Ext4)
    }

    pub fn name(&self) -> &'static str {
        match self {
            FsFlavor::Ext2 => "ext2",
            FsFlavor::Ext3 => "ext3",
            FsFlavor::Ext4 => "ext4",
        }
    }

    /// Returns `(inode_size, desc_size, csum_enabled, dir_csum_tail)` tuple.
    pub(crate) fn geometry(&self) -> (u16, u16, bool, usize) {
        let csum_enabled = matches!(self, FsFlavor::Ext4);
        let inode_size: u16 = if csum_enabled { 256 } else { 128 };
        let desc_size: u16 = if csum_enabled { 64 } else { 32 };
        let dir_csum_tail: usize = if csum_enabled { 12 } else { 0 };
        (inode_size, desc_size, csum_enabled, dir_csum_tail)
    }
}

/// RO_COMPAT bits that change how the filesystem is *read*, and so
/// cannot be ignored even by a read-only mount. None, today.
///
/// The compatibility model says an unknown RO_COMPAT bit is safe to
/// mount read-only: it describes something a reader may ignore. A bit
/// that invalidated how this reader locates bytes would be the exception
/// and would belong here.
///
/// `BIGALLOC` was here, on the premise that it replaces
/// `s_blocks_per_group` as the group stride. It does not (#75). The kernel
/// refuses a volume whose `s_blocks_per_group` is not `s_clusters_per_group`
/// times the cluster ratio, so group offsets, inode tables and extents --
/// everything a read locates -- stay in blocks. What counts clusters is the
/// block bitmaps and the descriptors' free counts, which only allocation and
/// the audits read. So a bigalloc volume reads as any other: it is in
/// [`SUPPORTED_RO_COMPAT`] and not [`MAINTAINED_RO_COMPAT`], which refuses
/// writes, and `fsck::audit` and `verify::verify` refuse it by name.
pub const READ_BREAKING_RO_COMPAT: u32 = 0;

/// RO_COMPAT bits this driver **maintains**, as opposed to tolerates.
///
/// [`SUPPORTED_RO_COMPAT`] is the read set: bits a reader may ignore.
/// This is the smaller write set, and the difference between them is
/// the point.
///
/// - `QUOTA` and `PROJECT` are absent because nothing here touches the
///   quota inodes. The string "quota" appears in this file and nowhere
///   else in `src/`. A create on a quota-enabled volume charges nobody
///   for it and leaves counters that no longer describe the filesystem,
///   which `e2fsck` reports later as a mismatch the user cannot connect
///   to having plugged the disk into a Mac.
/// - `ORPHAN_PRESENT` is absent for the same reason: the orphan file is
///   read (see `Filesystem::orphan_list`) and not maintained.
/// - `GDT_CSUM` is present, and was not at first. Its group-descriptor
///   checksums are crc16 rather than `METADATA_CSUM`'s crc32c, and until
///   [`crate::checksum::group_desc_csum`] existed nothing here computed
///   them, so every descriptor a write touched was left stale. It is
///   what `mke2fs` produced by default before 1.43, so the ordinary
///   shape of an older disk.
///
/// Everything else in [`SUPPORTED_RO_COMPAT`] describes structures the
/// write paths already keep correct.
pub const MAINTAINED_RO_COMPAT: u32 = RoCompat::SPARSE_SUPER.bits()
    | RoCompat::LARGE_FILE.bits()
    | RoCompat::HUGE_FILE.bits()
    | RoCompat::DIR_NLINK.bits()
    | RoCompat::EXTRA_ISIZE.bits()
    | RoCompat::GDT_CSUM.bits()
    | RoCompat::METADATA_CSUM.bits();

/// The RO_COMPAT bits on this volume that a write here would not keep
/// consistent: anything outside [`MAINTAINED_RO_COMPAT`].
///
/// # Why this is separate from [`check_mountable`]
///
/// `RO_COMPAT` names one rule with two halves — a reader that does not
/// know the bit may READ, and must not WRITE. `check_mountable` decides
/// the first half and its comments said so ("Check whether the
/// filesystem can be mounted read-only"), but this driver has not been
/// read-only for a long time: it creates, unlinks, writes, truncates,
/// and formats. So the second half was not enforced anywhere, and a
/// volume with an unrecognised bit was mounted writable with a test
/// pinning that it should be.
///
/// Zero means the volume may be written.
pub fn unmaintained_ro_compat(feature_ro_compat: u32) -> u32 {
    feature_ro_compat & !MAINTAINED_RO_COMPAT
}

/// Check whether the filesystem can be mounted read-only.
/// Returns Err with the unsupported bits if not.
pub fn check_mountable(feature_incompat: u32, feature_ro_compat: u32) -> crate::error::Result<()> {
    let unsupported_incompat = feature_incompat & !SUPPORTED_INCOMPAT;
    if unsupported_incompat != 0 {
        return Err(crate::error::Error::UnsupportedIncompat(
            unsupported_incompat,
        ));
    }
    // Most RO_COMPAT bits are safe to ignore on a read-only mount, per
    // the compatibility model. The exceptions are the ones that change
    // how bytes are located — see READ_BREAKING_RO_COMPAT.
    let breaking = feature_ro_compat & READ_BREAKING_RO_COMPAT;
    if breaking != 0 {
        return Err(crate::error::Error::UnsupportedRoCompat(breaking));
    }
    Ok(())
}

#[cfg(test)]
mod mountability_tests {
    use super::*;

    /// A bigalloc filesystem mounts to be read, and is not written (#75):
    /// reads locate everything in blocks, and only allocation counts
    /// clusters.
    #[test]
    fn a_bigalloc_filesystem_reads_and_is_not_written() {
        check_mountable(0, RoCompat::BIGALLOC.bits())
            .expect("bigalloc leaves every read-side offset in blocks");
        assert_eq!(
            unmaintained_ro_compat(RoCompat::BIGALLOC.bits()),
            RoCompat::BIGALLOC.bits(),
            "a write would allocate blocks where the bitmap counts clusters"
        );
    }

    /// Every other RO_COMPAT bit still mounts. The point is a targeted
    /// refusal, not a stricter filesystem.
    /// Every tolerated RO_COMPAT bit mounts.
    ///
    /// Derived from `SUPPORTED_RO_COMPAT` rather than hand-listed. A
    /// hand-written list is a second place to remember a bit, and the
    /// one it forgot was GDT_CSUM: supported since the first release,
    /// never asserted, so a regression rejecting it would have passed
    /// this suite.
    #[test]
    fn the_other_ro_compat_bits_still_mount() {
        let tolerated = SUPPORTED_RO_COMPAT & !READ_BREAKING_RO_COMPAT;
        assert_ne!(tolerated, 0, "nothing to check — the masks are wrong");
        for shift in 0..32 {
            let bit = 1u32 << shift;
            if tolerated & bit == 0 {
                continue;
            }
            check_mountable(0, bit)
                .unwrap_or_else(|e| panic!("ro_compat {bit:#x} should mount read-only, got {e:?}"));
        }
    }

    /// An RO_COMPAT bit this crate has never heard of still mounts.
    ///
    /// That is the compatibility model working as intended, and the
    /// reason the refusal above has to be an explicit list rather than
    /// "anything outside SUPPORTED_RO_COMPAT".
    #[test]
    fn an_unknown_ro_compat_bit_still_mounts() {
        check_mountable(0, 0x8000_0000).expect("unknown RO_COMPAT is safe on a read-only mount");
    }

    /// The two masks must not contradict each other: a bit cannot be
    /// both supported and read-breaking.
    #[test]
    fn the_supported_and_breaking_masks_are_disjoint() {
        assert_eq!(
            SUPPORTED_RO_COMPAT & READ_BREAKING_RO_COMPAT,
            0,
            "a bit cannot be both tolerated and refused"
        );
    }

    /// An unsupported INCOMPAT bit is still refused — the existing
    /// behaviour, pinned so the RO_COMPAT change did not disturb it.
    #[test]
    fn an_unsupported_incompat_bit_is_still_refused() {
        assert!(check_mountable(0x8000_0000, 0).is_err());
    }
}
