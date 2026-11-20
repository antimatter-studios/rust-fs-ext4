//! The superblock and its backups, the group descriptor tables and their
//! copies, and inode records, each read where the format puts it and
//! returned with how far it can be believed.
//!
//! Where these structures are comes from the ext4 format documentation
//! (docs.kernel.org, "The Super Block", "Block Group Descriptors", "Block
//! Groups" / "Meta Block Groups", "Index Nodes") and is pinned against
//! `dumpe2fs` by `tests/group_layout_oracle.rs` and
//! `tests/salvage_structures_oracle.rs`:
//!
//! - the primary superblock is the 1024 bytes at byte 1024;
//! - each backup is the first 1024 bytes of the first block of a group that
//!   [`Superblock::group_has_super`] names, that block being
//!   `s_first_data_block + group * s_blocks_per_group`, with
//!   `s_block_group_nr` (0x5A) set to the group;
//! - without `META_BG`, the descriptor table follows each superblock copy
//!   in the next block; with it, a meta group's one descriptor block is at
//!   the head of its first group, copied at the head of its second and last.

use super::{Damage, DamageKind, Extent, Integrity, SalvageError, Severity};
use crate::bgd::{BgdFlags, BlockGroupDescriptor};
use crate::checksum::{group_desc_csum, linux_crc32c, Checksummer};
use crate::features::{Incompat, RoCompat};
use crate::inode::Inode;
use crate::superblock::{
    Superblock, EXT4_ERROR_FS, EXT4_MAGIC, EXT4_VALID_FS, MAX_LOG_BLOCK_SIZE, SUPERBLOCK_OFFSET,
    SUPERBLOCK_SIZE,
};
use fs_core::BlockRead;

/// Where `s_checksum` sits in the superblock; the CRC covers the bytes before it.
const SB_CHECKSUM_OFFSET: usize = 0x3FC;
/// `s_feature_ro_compat`.
const SB_RO_COMPAT_OFFSET: usize = 0x64;
/// `s_block_group_nr`: the group a backup copy was written into.
const SB_BLOCK_GROUP_NR_OFFSET: usize = 0x5A;
/// `bg_checksum` in a group descriptor.
const BGD_CHECKSUM_OFFSET: usize = 0x1E;

/// The groups where `mke2fs`'s default sparse layout puts the first
/// backups (groups 1 and powers of 3, 5 and 7), probed when the primary
/// superblock is unreadable and nothing else says where a copy is.
const PROBE_GROUPS: [u64; 8] = [1, 3, 5, 7, 9, 25, 27, 49];

/// A fault seen in one structure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fault {
    pub kind: DamageKind,
    pub detail: String,
}

impl Fault {
    fn new(kind: DamageKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

/// One copy of the superblock, as found.
#[derive(Clone, Debug)]
pub struct SuperblockCopy {
    /// The group holding the copy: 0 for the primary.
    pub group: u64,
    /// Its 1024 bytes on the source.
    pub location: Extent,
    pub integrity: Integrity,
    /// The parsed superblock, when the bytes parse -- including when its
    /// checksum failed, since a copy with one wrong field still says where
    /// everything else is.
    pub superblock: Option<Superblock>,
    /// What is wrong with it, if anything.
    pub fault: Option<Fault>,
}

impl SuperblockCopy {
    /// Whether this copy parses and passes its checksum (or has none).
    pub fn is_sound(&self) -> bool {
        self.superblock.is_some() && self.integrity != Integrity::Failed
    }

    fn structure(&self) -> String {
        if self.group == 0 {
            "primary superblock".to_string()
        } else {
            format!("superblock backup in group {}", self.group)
        }
    }
}

/// Read the 1024 bytes at `offset` as a superblock copy held by `group`.
///
/// Never fails: an unreadable range, a wrong magic number or an
/// impossible field comes back as the copy's [`Fault`].
pub fn read_superblock_at(source: &dyn BlockRead, group: u64, offset: u64) -> SuperblockCopy {
    let location = Extent::new(offset, SUPERBLOCK_SIZE as u64);
    let failed = |fault: Fault| SuperblockCopy {
        group,
        location,
        integrity: Integrity::Failed,
        superblock: None,
        fault: Some(fault),
    };
    let mut raw = vec![0u8; SUPERBLOCK_SIZE];
    if let Err(error) = source.read_at(offset, &mut raw) {
        return failed(Fault::new(DamageKind::Unreadable, error.to_string()));
    }
    let magic = u16::from_le_bytes([raw[0x38], raw[0x39]]);
    if magic != EXT4_MAGIC {
        return failed(Fault::new(
            DamageKind::Signature,
            format!("magic {magic:#06x}, expected {EXT4_MAGIC:#06x}"),
        ));
    }
    let checksum = superblock_checksum(&raw);
    let superblock = match Superblock::parse(raw) {
        Ok(superblock) => superblock,
        Err(error) => return failed(Fault::new(DamageKind::Malformed, error.to_string())),
    };
    let (integrity, fault) = match checksum {
        None => (Integrity::Unchecked, None),
        Some((stored, computed)) if stored == computed => (Integrity::Verified, None),
        Some((stored, computed)) => (
            Integrity::Failed,
            Some(Fault::new(
                DamageKind::Checksum,
                format!("stored {stored:#010x}, computed {computed:#010x}"),
            )),
        ),
    };
    SuperblockCopy {
        group,
        location,
        integrity,
        superblock: Some(superblock),
        fault,
    }
}

/// The stored and computed superblock checksums, or `None` when the copy
/// does not declare `metadata_csum`. The CRC32C covers the bytes before
/// `s_checksum` from a seed of `~0` (format documentation, "The Super
/// Block", `s_checksum`).
fn superblock_checksum(raw: &[u8]) -> Option<(u32, u32)> {
    let ro_compat = u32::from_le_bytes(
        raw[SB_RO_COMPAT_OFFSET..SB_RO_COMPAT_OFFSET + 4]
            .try_into()
            .unwrap(),
    );
    if ro_compat & RoCompat::METADATA_CSUM.bits() == 0 {
        return None;
    }
    let stored = u32::from_le_bytes(
        raw[SB_CHECKSUM_OFFSET..SB_CHECKSUM_OFFSET + 4]
            .try_into()
            .unwrap(),
    );
    Some((stored, linux_crc32c(!0, &raw[..SB_CHECKSUM_OFFSET])))
}

/// The first block of `group`.
fn first_block_of_group(sb: &Superblock, group: u64) -> u64 {
    u64::from(sb.first_data_block) + group * u64::from(sb.blocks_per_group)
}

/// The byte offset of the superblock copy held by `group`.
fn superblock_offset(sb: &Superblock, group: u64) -> u64 {
    if group == 0 {
        SUPERBLOCK_OFFSET
    } else {
        first_block_of_group(sb, group) * u64::from(sb.block_size())
    }
}

/// Every place `sb`'s layout puts a superblock copy, the primary first,
/// each read and judged. Copies that run past the end of the source are
/// listed as unreadable rather than left out.
pub fn superblock_copies(source: &dyn BlockRead, sb: &Superblock) -> Vec<SuperblockCopy> {
    let mut copies = vec![read_superblock_at(source, 0, SUPERBLOCK_OFFSET)];
    for group in 1..sb.block_group_count() {
        if sb.group_has_super(group) {
            copies.push(read_superblock_at(
                source,
                group,
                superblock_offset(sb, group),
            ));
        }
    }
    copies
}

/// The superblock to take the volume's geometry from.
///
/// The primary when it is sound. Otherwise the first sound backup, found
/// through the primary's own geometry when that still parses, or by
/// probing the default sparse backup groups at every block size when it
/// does not. A primary that parses but fails its checksum is the last
/// resort before [`SalvageError::NoGeometry`].
pub fn find_superblock(source: &dyn BlockRead) -> Result<SuperblockCopy, SalvageError> {
    let primary = read_superblock_at(source, 0, SUPERBLOCK_OFFSET);
    if primary.is_sound() {
        return Ok(primary);
    }
    if let Some(sb) = &primary.superblock {
        if let Some(backup) = superblock_copies(source, sb)
            .into_iter()
            .skip(1)
            .find(SuperblockCopy::is_sound)
        {
            return Ok(backup);
        }
    }
    if let Some(backup) = probe_backups(source) {
        return Ok(backup);
    }
    if primary.superblock.is_some() {
        return Ok(primary);
    }
    Err(SalvageError::NoGeometry)
}

/// Look for a sound backup at the default layout's first backup groups,
/// for each block size the format allows, with the default
/// `blocks_per_group` of eight times the block size.
fn probe_backups(source: &dyn BlockRead) -> Option<SuperblockCopy> {
    let size = source.size_bytes();
    for log in 0..=MAX_LOG_BLOCK_SIZE {
        let block_size = 1024u64 << log;
        let blocks_per_group = 8 * block_size;
        let first_data_block = u64::from(block_size == 1024);
        for group in PROBE_GROUPS {
            let Some(offset) = group
                .checked_mul(blocks_per_group)
                .and_then(|b| b.checked_add(first_data_block))
                .and_then(|b| b.checked_mul(block_size))
            else {
                continue;
            };
            if offset.saturating_add(SUPERBLOCK_SIZE as u64) > size {
                continue;
            }
            let copy = read_superblock_at(source, group, offset);
            let fits = copy.superblock.as_ref().is_some_and(|sb| {
                u64::from(sb.block_size()) == block_size
                    && u64::from(sb.blocks_per_group) == blocks_per_group
            });
            if fits && copy.is_sound() {
                return Some(copy);
            }
        }
    }
    None
}

/// One group descriptor, from one copy of the table.
#[derive(Clone, Debug)]
pub struct DescriptorRecord {
    /// The group it describes.
    pub group: u64,
    /// 0 for the primary table, 1 and up for backups.
    pub copy: usize,
    pub location: Extent,
    pub integrity: Integrity,
    /// The parsed descriptor, kept when its checksum fails or a pointer is
    /// out of range: the other fields may still be right.
    pub descriptor: Option<BlockGroupDescriptor>,
    pub fault: Option<Fault>,
}

impl DescriptorRecord {
    fn structure(&self) -> String {
        if self.copy == 0 {
            format!("group descriptor {}", self.group)
        } else {
            format!("group descriptor {} (backup {})", self.group, self.copy)
        }
    }
}

/// How many copies of the descriptor tables `sb`'s layout keeps: the
/// primary plus the backups. Without `META_BG` every group holding a
/// superblock copy holds a whole table; with it each meta group keeps two
/// backups of its block, so a volume of both kinds of group counts the
/// larger. Not every group's descriptor is in every copy:
/// [`group_descriptors`] leaves out those it has no place for.
pub fn descriptor_table_copies(sb: &Superblock) -> usize {
    let shared = shared_table_holders(sb).len();
    if sb.feature_incompat & Incompat::META_BG.bits() != 0 {
        shared.max(3)
    } else {
        shared
    }
}

/// The groups holding a copy of the table that follows a superblock,
/// group 0 first.
fn shared_table_holders(sb: &Superblock) -> Vec<u64> {
    (0..sb.block_group_count())
        .filter(|&group| sb.group_has_super(group))
        .collect()
}

/// The meta group whose own block describes `group`, or `None` when the
/// table after the superblock does.
fn meta_group_of(sb: &Superblock, group: u64) -> Option<u64> {
    if sb.feature_incompat & Incompat::META_BG.bits() == 0 {
        return None;
    }
    let meta_group = group / sb.descs_per_block();
    (meta_group >= sb.first_meta_bg()).then_some(meta_group)
}

/// Where copy `copy` of `group`'s descriptor is: its block and the byte
/// offset in it, or `None` when that copy does not exist.
fn descriptor_place(
    sb: &Superblock,
    group: u64,
    copy: usize,
    holders: &[u64],
) -> Option<(u64, usize)> {
    if copy == 0 {
        return Some(sb.descriptor_location(group));
    }
    let per_block = sb.descs_per_block();
    let offset = ((group % per_block) * u64::from(sb.desc_size)) as usize;
    match meta_group_of(sb, group) {
        None => {
            let holder = *holders.get(copy)?;
            let block = first_block_of_group(sb, holder) + 1 + group / per_block;
            Some((block, offset))
        }
        Some(meta_group) => {
            let head = meta_group * per_block;
            let holder = match copy {
                1 => head + 1,
                2 => head + per_block - 1,
                _ => return None,
            };
            if holder >= sb.block_group_count() {
                return None;
            }
            let block = first_block_of_group(sb, holder) + u64::from(sb.group_has_super(holder));
            Some((block, offset))
        }
    }
}

/// Every group's descriptor from copy `copy` of the tables (0 is the
/// primary), each read and judged. A group this copy holds no descriptor
/// for is left out; one it holds but cannot read is listed as unreadable.
pub fn group_descriptors(
    source: &dyn BlockRead,
    sb: &Superblock,
    copy: usize,
) -> Vec<DescriptorRecord> {
    let holders = shared_table_holders(sb);
    let csum = Checksummer::from_superblock(sb);
    let block_size = u64::from(sb.block_size());
    let desc_size = usize::from(sb.desc_size.max(32));
    (0..sb.block_group_count())
        .filter_map(|group| {
            let (block, offset) = descriptor_place(sb, group, copy, &holders)?;
            let location = Extent::new(block * block_size + offset as u64, desc_size as u64);
            Some(read_descriptor(source, sb, &csum, group, copy, location))
        })
        .collect()
}

fn read_descriptor(
    source: &dyn BlockRead,
    sb: &Superblock,
    csum: &Checksummer,
    group: u64,
    copy: usize,
    location: Extent,
) -> DescriptorRecord {
    let mut raw = vec![0u8; location.length as usize];
    let mut record = DescriptorRecord {
        group,
        copy,
        location,
        integrity: Integrity::Failed,
        descriptor: None,
        fault: None,
    };
    if let Err(error) = source.read_at(location.offset, &mut raw) {
        record.fault = Some(Fault::new(DamageKind::Unreadable, error.to_string()));
        return record;
    }
    let descriptor = match BlockGroupDescriptor::parse(&raw, sb.desc_size.max(32)) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            record.fault = Some(Fault::new(DamageKind::Malformed, error.to_string()));
            return record;
        }
    };
    let stored = u16::from_le_bytes(
        raw[BGD_CHECKSUM_OFFSET..BGD_CHECKSUM_OFFSET + 2]
            .try_into()
            .unwrap(),
    );
    record.integrity = match group_desc_csum(sb, csum, group as u32, &raw) {
        None => Integrity::Unchecked,
        Some(computed) if computed == stored => Integrity::Verified,
        Some(computed) => {
            record.fault = Some(Fault::new(
                DamageKind::Checksum,
                format!("stored {stored:#06x}, computed {computed:#06x}"),
            ));
            Integrity::Failed
        }
    };
    if record.fault.is_none() {
        if let Some(problem) = pointer_problem(sb, &descriptor) {
            record.integrity = Integrity::Failed;
            record.fault = Some(Fault::new(DamageKind::Malformed, problem));
        }
    }
    record.descriptor = Some(descriptor);
    record
}

/// Why a descriptor's bitmap or inode table pointer cannot be right: zero,
/// or past the last block of the volume (with the inode table's whole
/// length).
fn pointer_problem(sb: &Superblock, descriptor: &BlockGroupDescriptor) -> Option<String> {
    let table_blocks = (u64::from(sb.inodes_per_group) * u64::from(sb.inode_size))
        .div_ceil(u64::from(sb.block_size()));
    for (name, block, length) in [
        ("block bitmap", descriptor.block_bitmap, 1),
        ("inode bitmap", descriptor.inode_bitmap, 1),
        ("inode table", descriptor.inode_table, table_blocks),
    ] {
        if block == 0 || block.saturating_add(length) > sb.blocks_count {
            return Some(format!(
                "{name} at block {block} lies outside the {}-block volume",
                sb.blocks_count
            ));
        }
    }
    None
}

/// One inode, as found in its group's inode table.
#[derive(Clone, Debug)]
pub struct InodeRecord {
    pub ino: u32,
    pub location: Extent,
    pub integrity: Integrity,
    /// The parsed inode, kept when its checksum fails.
    pub inode: Option<Inode>,
    /// Its `s_inode_size` bytes as read; empty when they could not be.
    pub raw: Vec<u8>,
    /// The group's descriptor marks its inode table uninitialized
    /// (`INODE_UNINIT`), so these bytes were never written as an inode.
    pub uninitialized: bool,
    pub fault: Option<Fault>,
}

/// Read inode `ino` from the inode table that `descriptors` (one copy of
/// the group descriptors, as [`group_descriptors`] returns them) places
/// for its group.
///
/// Fails only for an inode number the superblock does not allow. A group
/// with no usable descriptor, an unreadable table or a checksum mismatch
/// all come back as the record's [`Fault`].
pub fn read_inode(
    source: &dyn BlockRead,
    sb: &Superblock,
    descriptors: &[DescriptorRecord],
    ino: u32,
) -> Result<InodeRecord, SalvageError> {
    if ino == 0 || ino > sb.inodes_count {
        return Err(SalvageError::OutOfRange(format!(
            "inode {ino} (the volume has {})",
            sb.inodes_count
        )));
    }
    let inodes_per_group = u64::from(sb.inodes_per_group);
    let group = u64::from(ino - 1) / inodes_per_group;
    let index = u64::from(ino - 1) % inodes_per_group;
    let inode_size = u64::from(sb.inode_size);
    let mut record = InodeRecord {
        ino,
        location: Extent::new(0, inode_size),
        integrity: Integrity::Failed,
        inode: None,
        raw: Vec::new(),
        uninitialized: false,
        fault: None,
    };
    let Some(descriptor) = descriptors
        .iter()
        .filter(|record| record.group == group)
        .find_map(|record| record.descriptor.as_ref())
    else {
        record.fault = Some(Fault::new(
            DamageKind::Missing,
            format!("no descriptor for group {group} locates its inode table"),
        ));
        return Ok(record);
    };
    record.uninitialized = descriptor.flags().contains(BgdFlags::INODE_UNINIT);
    let Some(offset) = descriptor
        .inode_table
        .checked_mul(u64::from(sb.block_size()))
        .and_then(|o| o.checked_add(index * inode_size))
    else {
        record.fault = Some(Fault::new(
            DamageKind::Malformed,
            format!("inode table block {} overflows", descriptor.inode_table),
        ));
        return Ok(record);
    };
    record.location.offset = offset;
    let mut raw = vec![0u8; inode_size as usize];
    if let Err(error) = source.read_at(offset, &mut raw) {
        record.fault = Some(Fault::new(DamageKind::Unreadable, error.to_string()));
        return Ok(record);
    }
    match Inode::parse(&raw) {
        Ok(inode) => {
            let csum = Checksummer::from_superblock(sb);
            record.integrity = if !csum.enabled {
                Integrity::Unchecked
            } else if csum.verify_inode(ino, inode.generation, &raw) {
                Integrity::Verified
            } else {
                record.fault = Some(Fault::new(
                    DamageKind::Checksum,
                    format!("inode {ino} checksum does not match its bytes"),
                ));
                Integrity::Failed
            };
            record.inode = Some(inode);
        }
        Err(error) => {
            record.fault = Some(Fault::new(DamageKind::Malformed, error.to_string()));
        }
    }
    record.raw = raw;
    Ok(record)
}

/// The superblock fields every copy must agree on: the ones that say where
/// things are. Free counts, times and the mount state are updated in the
/// primary alone, so copies legitimately differ there.
fn geometry_fields(sb: &Superblock) -> [(&'static str, u64); 9] {
    [
        ("s_inodes_count", u64::from(sb.inodes_count)),
        ("s_blocks_count", sb.blocks_count),
        ("s_first_data_block", u64::from(sb.first_data_block)),
        ("s_log_block_size", u64::from(sb.log_block_size)),
        ("s_blocks_per_group", u64::from(sb.blocks_per_group)),
        ("s_inodes_per_group", u64::from(sb.inodes_per_group)),
        ("s_inode_size", u64::from(sb.inode_size)),
        ("s_desc_size", u64::from(sb.desc_size)),
        ("s_feature_incompat", u64::from(sb.feature_incompat)),
    ]
}

/// What is wrong with the superblock copies and the group descriptor
/// tables: each fault found, and each disagreement between copies.
///
/// A fault in the primary is a [`Severity::Warning`] when a sound copy
/// covers it and an [`Severity::Error`] when none does. The state the
/// superblock records -- not cleanly unmounted, errors seen, a journal to
/// recover -- is reported as found.
pub fn structure_damage(source: &dyn BlockRead) -> Result<Vec<Damage>, SalvageError> {
    let chosen = find_superblock(source)?;
    let sb = chosen
        .superblock
        .clone()
        .expect("find_superblock returns a copy that parses");
    let mut damage = Vec::new();
    let copies = superblock_copies(source, &sb);
    let backup_sound = copies.iter().skip(1).any(SuperblockCopy::is_sound);
    for copy in &copies {
        if let Some(fault) = &copy.fault {
            let covered = copy.group != 0 || backup_sound;
            damage.push(Damage {
                kind: fault.kind,
                severity: if covered {
                    Severity::Warning
                } else {
                    Severity::Error
                },
                location: copy.location,
                structure: copy.structure(),
                detail: fault.detail.clone(),
            });
        }
        let Some(other) = &copy.superblock else {
            continue;
        };
        if copy.group != 0 {
            let recorded = u16::from_le_bytes(
                other.raw[SB_BLOCK_GROUP_NR_OFFSET..SB_BLOCK_GROUP_NR_OFFSET + 2]
                    .try_into()
                    .unwrap(),
            );
            if u64::from(recorded) != copy.group {
                damage.push(Damage {
                    kind: DamageKind::Mismatch,
                    severity: Severity::Warning,
                    location: copy.location,
                    structure: copy.structure(),
                    detail: format!("s_block_group_nr is {recorded}"),
                });
            }
        }
        let differing: Vec<String> = geometry_fields(&sb)
            .iter()
            .zip(geometry_fields(other).iter())
            .filter(|(a, b)| a.1 != b.1)
            .map(|(a, b)| format!("{} {} here, {} in the copy used", a.0, b.1, a.1))
            .collect();
        if !differing.is_empty() {
            damage.push(Damage {
                kind: DamageKind::Mismatch,
                severity: Severity::Warning,
                location: copy.location,
                structure: copy.structure(),
                detail: differing.join("; "),
            });
        }
    }
    damage.extend(recorded_state(&sb, chosen.location));
    damage.extend(descriptor_damage(source, &sb));
    Ok(damage)
}

/// The state the superblock records about itself.
fn recorded_state(sb: &Superblock, location: Extent) -> Vec<Damage> {
    let mut found = Vec::new();
    let mut note = |severity, detail: &str| {
        found.push(Damage {
            kind: DamageKind::Recorded,
            severity,
            location,
            structure: "superblock".to_string(),
            detail: detail.to_string(),
        })
    };
    if sb.state & EXT4_VALID_FS == 0 {
        note(
            Severity::Notice,
            "not cleanly unmounted (s_state lacks VALID_FS)",
        );
    }
    if sb.state & EXT4_ERROR_FS != 0 {
        note(
            Severity::Warning,
            "errors were detected (s_state has ERROR_FS)",
        );
    }
    if sb.feature_incompat & Incompat::RECOVER.bits() != 0 {
        note(
            Severity::Notice,
            "the journal needs recovery (RECOVER is set)",
        );
    }
    found
}

/// Faults in the primary descriptor table, and where a backup's bitmap or
/// inode table pointers disagree with the primary's.
fn descriptor_damage(source: &dyn BlockRead, sb: &Superblock) -> Vec<Damage> {
    let mut damage = Vec::new();
    let primary = group_descriptors(source, sb, 0);
    let backup = if descriptor_table_copies(sb) > 1 {
        group_descriptors(source, sb, 1)
    } else {
        Vec::new()
    };
    let backup_of = |group: u64| backup.iter().find(|record| record.group == group);
    for record in &primary {
        let sound_backup = backup_of(record.group)
            .is_some_and(|b| b.descriptor.is_some() && b.integrity != Integrity::Failed);
        if let Some(fault) = &record.fault {
            damage.push(Damage {
                kind: fault.kind,
                severity: if sound_backup {
                    Severity::Warning
                } else {
                    Severity::Error
                },
                location: record.location,
                structure: record.structure(),
                detail: fault.detail.clone(),
            });
        }
        let (Some(here), Some(there)) = (
            record.descriptor.as_ref(),
            backup_of(record.group).and_then(|b| b.descriptor.as_ref()),
        ) else {
            continue;
        };
        let pointers = |d: &BlockGroupDescriptor| (d.block_bitmap, d.inode_bitmap, d.inode_table);
        if pointers(here) != pointers(there) {
            damage.push(Damage {
                kind: DamageKind::Mismatch,
                severity: Severity::Warning,
                location: record.location,
                structure: record.structure(),
                detail: format!(
                    "block bitmap, inode bitmap and inode table {:?} here, {:?} in backup 1",
                    pointers(here),
                    pointers(there)
                ),
            });
        }
    }
    damage
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_io::BlockDevice;
    use crate::features::FsFlavor;
    use crate::mkfs::format_filesystem_with_flavor;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// The crate's `mkfs` writes only metadata, so a volume is kept as the
    /// chunks written to it: four groups of 2 KiB blocks are 128 MiB, of
    /// which a few hundred KiB are ever stored.
    const CHUNK: u64 = 4096;

    struct Memory {
        size: u64,
        chunks: Mutex<BTreeMap<u64, Vec<u8>>>,
    }

    impl Memory {
        fn format(flavor: FsFlavor, mib: u64, block_size: u32) -> Self {
            let memory = Memory {
                size: mib << 20,
                chunks: Mutex::new(BTreeMap::new()),
            };
            format_filesystem_with_flavor(
                &memory,
                Some("salvage"),
                Some([7; 16]),
                mib << 20,
                block_size,
                flavor,
            )
            .unwrap();
            memory
        }

        fn copy(&self, offset: u64, buf: &mut [u8]) {
            let chunks = self.chunks.lock().unwrap();
            let mut done = 0;
            while done < buf.len() {
                let at = offset + done as u64;
                let within = (at % CHUNK) as usize;
                let take = (CHUNK as usize - within).min(buf.len() - done);
                let out = &mut buf[done..done + take];
                match chunks.get(&(at / CHUNK)) {
                    Some(chunk) => out.copy_from_slice(&chunk[within..within + take]),
                    None => out.fill(0),
                }
                done += take;
            }
        }

        fn poke(&self, offset: u64, bytes: &[u8]) {
            let mut chunks = self.chunks.lock().unwrap();
            let mut done = 0;
            while done < bytes.len() {
                let at = offset + done as u64;
                let within = (at % CHUNK) as usize;
                let take = (CHUNK as usize - within).min(bytes.len() - done);
                let part = &bytes[done..done + take];
                if let Some(chunk) = chunks.get_mut(&(at / CHUNK)) {
                    chunk[within..within + take].copy_from_slice(part);
                } else if part.iter().any(|&b| b != 0) {
                    let mut chunk = vec![0; CHUNK as usize];
                    chunk[within..within + take].copy_from_slice(part);
                    chunks.insert(at / CHUNK, chunk);
                }
                done += take;
            }
        }

        fn flip(&self, offset: u64) {
            let mut byte = [0u8];
            self.copy(offset, &mut byte);
            self.poke(offset, &[byte[0] ^ 0xFF]);
        }
    }

    impl BlockDevice for Memory {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> crate::error::Result<()> {
            self.copy(offset, buf);
            Ok(())
        }
        fn write_at(&self, offset: u64, buf: &[u8]) -> crate::error::Result<()> {
            self.poke(offset, buf);
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            self.size
        }
        fn is_writable(&self) -> bool {
            true
        }
    }

    impl fs_core::BlockRead for Memory {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
            match offset.checked_add(buf.len() as u64) {
                Some(end) if end <= self.size => {
                    self.copy(offset, buf);
                    Ok(())
                }
                _ => Err(fs_core::Error::OutOfBounds {
                    offset,
                    len: buf.len() as u64,
                    size: self.size,
                }),
            }
        }
        fn size_bytes(&self) -> u64 {
            self.size
        }
    }

    /// 2 KiB blocks and 16384 blocks per group: 128 MiB is four groups,
    /// with backups in groups 1 and 3.
    fn ext4() -> Memory {
        Memory::format(FsFlavor::Ext4, 128, 2048)
    }

    /// The crate's `mkfs` makes ext2 in one group only.
    fn ext2() -> Memory {
        Memory::format(FsFlavor::Ext2, 8, 1024)
    }

    fn source(memory: &Memory) -> &dyn BlockRead {
        memory
    }

    fn sb_of(memory: &Memory) -> Superblock {
        find_superblock(source(memory)).unwrap().superblock.unwrap()
    }

    #[test]
    fn a_fresh_volume_lists_every_copy_sound_and_no_damage() {
        let memory = ext4();
        let chosen = find_superblock(source(&memory)).unwrap();
        assert_eq!(chosen.group, 0);
        assert_eq!(chosen.integrity, Integrity::Verified);
        let sb = chosen.superblock.unwrap();
        let copies = superblock_copies(source(&memory), &sb);
        let groups: Vec<u64> = copies.iter().map(|c| c.group).collect();
        assert_eq!(groups, [0, 1, 3]);
        assert!(copies.iter().all(SuperblockCopy::is_sound), "{copies:?}");
        assert_eq!(copies[1].location.offset, 16384 * 2048);
        assert_eq!(structure_damage(source(&memory)).unwrap(), Vec::new());
    }

    #[test]
    fn a_primary_with_a_wrong_magic_falls_back_to_the_first_backup() {
        let memory = ext4();
        memory.poke(SUPERBLOCK_OFFSET + 0x38, &[0, 0]);
        let chosen = find_superblock(source(&memory)).unwrap();
        assert_eq!(chosen.group, 1);
        let damage = structure_damage(source(&memory)).unwrap();
        assert!(
            damage.iter().any(|d| d.kind == DamageKind::Signature
                && d.severity == Severity::Warning
                && d.structure == "primary superblock"),
            "{damage:?}"
        );
    }

    #[test]
    fn a_primary_whose_checksum_fails_is_parsed_and_reported() {
        let memory = ext4();
        memory.flip(SUPERBLOCK_OFFSET + 0x78); // a byte of s_volume_name
        let primary = read_superblock_at(source(&memory), 0, SUPERBLOCK_OFFSET);
        assert_eq!(primary.integrity, Integrity::Failed);
        assert!(primary.superblock.is_some());
        assert_eq!(primary.fault.as_ref().unwrap().kind, DamageKind::Checksum);
        assert_eq!(find_superblock(source(&memory)).unwrap().group, 1);
    }

    #[test]
    fn a_zeroed_primary_is_found_again_by_probing_the_backups() {
        let memory = ext4();
        memory.poke(SUPERBLOCK_OFFSET, &[0; SUPERBLOCK_SIZE]);
        let chosen = find_superblock(source(&memory)).unwrap();
        assert_eq!(chosen.group, 1);
        assert_eq!(chosen.integrity, Integrity::Verified);
    }

    #[test]
    fn with_every_copy_destroyed_there_is_no_geometry() {
        let memory = ext4();
        let sb = sb_of(&memory);
        for copy in superblock_copies(source(&memory), &sb) {
            memory.poke(copy.location.offset + 0x38, &[0, 0]);
        }
        assert!(matches!(
            find_superblock(source(&memory)),
            Err(SalvageError::NoGeometry)
        ));
    }

    #[test]
    fn a_backup_that_disagrees_on_geometry_is_a_mismatch() {
        let memory = ext4();
        let sb = sb_of(&memory);
        let backup = &superblock_copies(source(&memory), &sb)[2];
        // s_inodes_count in the copy, with its checksum left to fail too.
        memory.poke(backup.location.offset, &1u32.to_le_bytes());
        let damage = structure_damage(source(&memory)).unwrap();
        assert!(
            damage.iter().any(|d| d.kind == DamageKind::Mismatch
                && d.structure == "superblock backup in group 3"
                && d.detail.contains("s_inodes_count")),
            "{damage:?}"
        );
    }

    #[test]
    fn descriptors_come_from_the_primary_and_each_backup_table() {
        let memory = ext4();
        let sb = sb_of(&memory);
        assert_eq!(descriptor_table_copies(&sb), 3);
        let primary = group_descriptors(source(&memory), &sb, 0);
        let backup = group_descriptors(source(&memory), &sb, 2);
        assert_eq!(primary.len(), 4);
        assert_eq!(backup.len(), 4);
        for (a, b) in primary.iter().zip(&backup) {
            assert_eq!(a.integrity, Integrity::Verified, "{a:?}");
            assert_eq!(b.integrity, Integrity::Verified, "{b:?}");
            let (a, b) = (
                a.descriptor.as_ref().unwrap(),
                b.descriptor.as_ref().unwrap(),
            );
            assert_eq!(a.inode_table, b.inode_table);
        }
        assert_eq!(backup[0].location.offset, (3 * 16384 + 1) * 2048);
    }

    #[test]
    fn a_damaged_descriptor_is_kept_and_covered_by_its_backup() {
        let memory = ext4();
        let sb = sb_of(&memory);
        let primary = group_descriptors(source(&memory), &sb, 0);
        memory.flip(primary[1].location.offset + 0x0C); // free blocks count
        let again = group_descriptors(source(&memory), &sb, 0);
        assert_eq!(again[1].integrity, Integrity::Failed);
        assert!(again[1].descriptor.is_some());
        let damage = structure_damage(source(&memory)).unwrap();
        assert!(
            damage.iter().any(|d| d.kind == DamageKind::Checksum
                && d.severity == Severity::Warning
                && d.structure == "group descriptor 1"),
            "{damage:?}"
        );
    }

    #[test]
    fn a_pointer_outside_the_volume_is_malformed_on_a_volume_without_checksums() {
        let memory = ext2();
        let sb = sb_of(&memory);
        let primary = group_descriptors(source(&memory), &sb, 0);
        assert_eq!(primary[0].integrity, Integrity::Unchecked);
        memory.poke(primary[0].location.offset + 0x08, &u32::MAX.to_le_bytes());
        let again = group_descriptors(source(&memory), &sb, 0);
        assert_eq!(again[0].integrity, Integrity::Failed);
        assert_eq!(again[0].fault.as_ref().unwrap().kind, DamageKind::Malformed);
    }

    #[test]
    fn the_root_inode_reads_verified_and_a_damaged_one_reads_failed() {
        let memory = ext4();
        let sb = sb_of(&memory);
        let descriptors = group_descriptors(source(&memory), &sb, 0);
        let root = read_inode(source(&memory), &sb, &descriptors, 2).unwrap();
        assert_eq!(root.integrity, Integrity::Verified);
        assert!(root.inode.as_ref().unwrap().is_dir());
        memory.flip(root.location.offset + 0x04); // i_size_lo
        let damaged = read_inode(source(&memory), &sb, &descriptors, 2).unwrap();
        assert_eq!(damaged.integrity, Integrity::Failed);
        assert!(damaged.inode.is_some());
    }

    #[test]
    fn an_inode_on_a_volume_without_checksums_is_unchecked() {
        let memory = ext2();
        let sb = sb_of(&memory);
        let descriptors = group_descriptors(source(&memory), &sb, 0);
        let root = read_inode(source(&memory), &sb, &descriptors, 2).unwrap();
        assert_eq!(root.integrity, Integrity::Unchecked);
        assert!(root.inode.unwrap().is_dir());
    }

    #[test]
    fn inode_zero_and_inodes_past_the_count_are_refused() {
        let memory = ext4();
        let sb = sb_of(&memory);
        let descriptors = group_descriptors(source(&memory), &sb, 0);
        assert!(read_inode(source(&memory), &sb, &descriptors, 0).is_err());
        assert!(read_inode(source(&memory), &sb, &descriptors, sb.inodes_count + 1).is_err());
    }

    #[test]
    fn an_inode_whose_group_has_no_descriptor_is_missing_not_refused() {
        let memory = ext4();
        let sb = sb_of(&memory);
        let record = read_inode(source(&memory), &sb, &[], 2).unwrap();
        assert_eq!(record.fault.unwrap().kind, DamageKind::Missing);
    }

    #[test]
    fn the_recorded_state_is_reported() {
        let memory = ext4();
        memory.poke(SUPERBLOCK_OFFSET + 0x3A, &EXT4_ERROR_FS.to_le_bytes());
        let primary = read_superblock_at(source(&memory), 0, SUPERBLOCK_OFFSET);
        // The poke broke the checksum, so the geometry comes from a backup,
        // whose state is clean: read the recorded state from the primary.
        let damage = recorded_state(&primary.superblock.unwrap(), primary.location);
        let details: Vec<&str> = damage.iter().map(|d| d.detail.as_str()).collect();
        assert_eq!(details.len(), 2, "{details:?}");
    }
}
