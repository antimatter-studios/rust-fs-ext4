//! Directory blocks read without trusting their record lengths, keeping
//! live entries, the deleted entries still legible in the slack after
//! them, and where the chain of records broke.
//!
//! The layout is the format documentation's ("Directory Entries": `struct
//! ext4_dir_entry_2`, its 4-byte alignment, `ext4_dir_entry_tail`, and the
//! hash tree's root and interior blocks, which present themselves as
//! entries whose record lengths cover the index). Removing an entry joins
//! its record onto the one before by growing that record's length, or
//! zeroes its inode number when it is first in the block. Whether the
//! removed bytes survive depends on the writer: the kernel in the harness
//! VM zeroes an entry it removes, but splitting a hashed directory's leaf
//! leaves the moved entries' old copies in the slack, and `debugfs ls -d`
//! lists exactly the deleted entries found here
//! (`tests/salvage_maps_oracle.rs`).

use super::maps::{map_inode, BlockMap};
use super::structures::{Fault, InodeRecord};
use super::{DamageKind, Extent, Integrity};
use crate::checksum::{Checksummer, DIR_ENTRY_TAIL_FILE_TYPE};
use crate::inode::Inode;
use crate::superblock::Superblock;
use fs_core::BlockRead;

/// The fixed part of a directory entry: inode, record length, name length,
/// file type.
const ENTRY_HEADER: usize = 8;
/// The checksum tail that ends a checksummed directory block.
const TAIL_BYTES: usize = 12;
/// The largest file type a directory entry records (a socket).
const MAX_FILE_TYPE: u8 = 7;
/// The parent's inode number at the start of an inline directory.
const INLINE_PARENT_BYTES: usize = 4;

/// One entry found in a directory block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundEntry {
    /// Its byte offset in the block.
    pub offset: usize,
    pub inode: u32,
    pub name: Vec<u8>,
    /// The `file_type` byte, 0 when the volume does not record types.
    pub file_type: u8,
    /// Reached through the chain of record lengths and not removed. A
    /// deleted entry is one recovered from the slack, or one whose inode
    /// number was zeroed.
    pub live: bool,
}

/// What one directory block held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirBlockScan {
    pub entries: Vec<FoundEntry>,
    /// The checksum tail's verdict: `Unchecked` when the block has none.
    pub integrity: Integrity,
    /// Where the record chain broke, and why, by offset in the block.
    pub faults: Vec<(usize, Fault)>,
}

/// The context a block is judged in.
#[derive(Clone, Copy, Debug)]
pub struct DirContext {
    /// `s_feature_incompat` has `FILETYPE`: the byte after the name length
    /// is a type, not the high byte of a longer name length.
    pub has_file_type: bool,
    /// `s_inodes_count`: no entry names an inode past it.
    pub inodes_count: u32,
    /// The checksum seed, and the directory's inode number and generation,
    /// which the tail checksum covers.
    pub csum: Checksummer,
    pub ino: u32,
    pub generation: u32,
    /// The directory is hashed (`EXT4_INDEX_FL`): its first block is the
    /// index root, and some others may be interior index nodes.
    pub indexed: bool,
}

impl DirContext {
    pub fn new(sb: &Superblock, ino: u32, inode: &Inode) -> Self {
        Self {
            has_file_type: sb.feature_incompat & crate::features::Incompat::FILETYPE.bits() != 0,
            inodes_count: sb.inodes_count,
            csum: Checksummer::from_superblock(sb),
            ino,
            generation: inode.generation,
            indexed: inode.flags & crate::inode::InodeFlags::INDEX.bits() != 0,
        }
    }
}

/// A hashed directory's index blocks, which present themselves as entries
/// whose record lengths cover the index (format documentation, "Hash Tree
/// Directories"): the root is `.` (12 bytes) then `..` reaching the end of
/// the block; an interior node is one empty record of the whole block.
fn index_block(block: &[u8], context: &DirContext) -> Option<DirBlockScan> {
    if !context.indexed {
        return None;
    }
    let first = raw_at(block, 0, context.has_file_type)?;
    let mut scan = DirBlockScan {
        entries: Vec::new(),
        integrity: Integrity::Unchecked,
        faults: Vec::new(),
    };
    if first.inode == 0 && first.rec_len == block.len() && first.name.is_empty() {
        return Some(scan);
    }
    let second = raw_at(block, 12, context.has_file_type)?;
    let is_root = first.name == b"."
        && first.rec_len == 12
        && second.name == b".."
        && second.rec_len == block.len() - 12;
    if !is_root {
        return None;
    }
    scan.entries.push(found(0, &first, first.inode != 0));
    scan.entries.push(found(12, &second, second.inode != 0));
    Some(scan)
}

/// An entry's header and name as stored at `offset`, without judging them.
struct Raw<'a> {
    inode: u32,
    rec_len: usize,
    name: &'a [u8],
    file_type: u8,
}

fn raw_at(block: &[u8], offset: usize, has_file_type: bool) -> Option<Raw<'_>> {
    let header = block.get(offset..offset + ENTRY_HEADER)?;
    let inode = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let rec_len = usize::from(u16::from_le_bytes(header[4..6].try_into().unwrap()));
    let (name_len, file_type) = if has_file_type {
        (usize::from(header[6]), header[7])
    } else {
        (usize::from(header[6]) | usize::from(header[7]) << 8, 0)
    };
    let name = block.get(offset + ENTRY_HEADER..offset + ENTRY_HEADER + name_len)?;
    Some(Raw {
        inode,
        rec_len,
        name,
        file_type,
    })
}

/// The space an entry with an `n`-byte name needs: the header and the
/// name, padded to 4 bytes.
fn needed(name_len: usize) -> usize {
    ENTRY_HEADER + name_len.div_ceil(4) * 4
}

fn plausible_name(name: &[u8]) -> bool {
    !name.is_empty() && !name.contains(&0) && !name.contains(&b'/')
}

/// Whether the bytes at `offset` read as an entry nothing contradicts: an
/// inode on the volume, a legal name, a known type, and a record length
/// that holds the name and stays in the block.
fn plausible(raw: &Raw<'_>, offset: usize, block_len: usize, context: &DirContext) -> bool {
    raw.inode != 0
        && raw.inode <= context.inodes_count
        && plausible_name(raw.name)
        && (!context.has_file_type || (1..=MAX_FILE_TYPE).contains(&raw.file_type))
        && raw.rec_len >= needed(raw.name.len())
        && raw.rec_len.is_multiple_of(4)
        && offset + raw.rec_len <= block_len
}

/// Read one directory block's entries, live and deleted. A hashed
/// directory's index blocks yield their `.` and `..` and nothing from the
/// index itself.
pub fn scan_dir_block(block: &[u8], context: &DirContext) -> DirBlockScan {
    if let Some(scan) = index_block(block, context) {
        return scan;
    }
    let mut scan = DirBlockScan {
        entries: Vec::new(),
        integrity: Integrity::Unchecked,
        faults: Vec::new(),
    };
    let mut end = block.len();
    if crate::dir::has_csum_tail(block) {
        end -= TAIL_BYTES;
        if context.csum.enabled {
            scan.integrity =
                if context
                    .csum
                    .verify_dir_entry_tail(context.ino, context.generation, block)
                {
                    Integrity::Verified
                } else {
                    scan.faults.push((
                        end,
                        Fault {
                            kind: DamageKind::Checksum,
                            detail: "the directory block's tail checksum does not match"
                                .to_string(),
                        },
                    ));
                    Integrity::Failed
                };
        }
    }
    let mut offset = 0;
    while offset + ENTRY_HEADER <= end {
        let Some(raw) = raw_at(&block[..end], offset, context.has_file_type) else {
            scan.faults
                .push((offset, malformed("the name runs past the block")));
            offset = resync(block, offset + 4, end, context);
            continue;
        };
        let header_ok = raw.rec_len >= ENTRY_HEADER
            && raw.rec_len.is_multiple_of(4)
            && offset + raw.rec_len <= end
            && raw.rec_len >= needed(raw.name.len());
        if !header_ok {
            scan.faults.push((
                offset,
                malformed(&format!(
                    "record length {} with a {}-byte name",
                    raw.rec_len,
                    raw.name.len()
                )),
            ));
            offset = resync(block, offset + 4, end, context);
            continue;
        }
        let is_tail = raw.inode == 0
            && raw.rec_len == TAIL_BYTES
            && raw.name.is_empty()
            && raw.file_type == DIR_ENTRY_TAIL_FILE_TYPE;
        if !is_tail && plausible_name(raw.name) {
            if raw.inode != 0 {
                scan.entries.push(found(offset, &raw, true));
            } else if raw.inode == 0 && offset == 0 {
                // A first entry removed by zeroing its inode keeps its name;
                // the inode number is gone with it.
                scan.entries.push(found(offset, &raw, false));
            }
        }
        slack(
            block,
            offset + needed(raw.name.len()),
            offset + raw.rec_len,
            context,
            &mut scan,
        );
        offset += raw.rec_len;
    }
    scan
}

fn malformed(detail: &str) -> Fault {
    Fault {
        kind: DamageKind::Malformed,
        detail: detail.to_string(),
    }
}

fn found(offset: usize, raw: &Raw<'_>, live: bool) -> FoundEntry {
    FoundEntry {
        offset,
        inode: raw.inode,
        name: raw.name.to_vec(),
        file_type: raw.file_type,
        live,
    }
}

/// Deleted entries in `from..to`, the space a live record covers beyond
/// its own name.
fn slack(block: &[u8], from: usize, to: usize, context: &DirContext, scan: &mut DirBlockScan) {
    let mut offset = from;
    while offset + ENTRY_HEADER <= to {
        match raw_at(&block[..to], offset, context.has_file_type) {
            Some(raw) if plausible(&raw, offset, to, context) => {
                scan.entries.push(found(offset, &raw, false));
                offset += needed(raw.name.len());
            }
            _ => offset += 4,
        }
    }
}

/// After a broken record: the next offset holding a plausible entry. The
/// chain picks up again there; the break itself is the caller's fault.
fn resync(block: &[u8], mut offset: usize, end: usize, context: &DirContext) -> usize {
    while offset + ENTRY_HEADER <= end {
        if let Some(raw) = raw_at(&block[..end], offset, context.has_file_type) {
            if plausible(&raw, offset, end, context) {
                return offset;
            }
        }
        offset += 4;
    }
    end
}

/// Every entry of a directory, live and deleted, from each block its map
/// reaches, or from the inode itself for an inline directory.
#[derive(Clone, Debug)]
pub struct DirectoryScan {
    pub map: BlockMap,
    /// Each block read, by its logical number, and what it held.
    pub blocks: Vec<(u64, Extent, DirBlockScan)>,
    /// For an inline directory, the parent's inode number, which the
    /// format keeps in place of a `..` entry.
    pub inline_parent: Option<u32>,
}

impl DirectoryScan {
    /// Every entry found, in block order.
    pub fn entries(&self) -> impl Iterator<Item = &FoundEntry> {
        self.blocks
            .iter()
            .flat_map(|(_, _, scan)| scan.entries.iter())
    }
}

/// Read a directory inode without trusting it: its map is followed as
/// [`map_inode`] does, and each block is scanned as [`scan_dir_block`]
/// does. A block that cannot be read is recorded with an empty scan and an
/// unreadable fault. Uninitialized extents hold no entries and are skipped.
pub fn read_directory(
    source: &dyn BlockRead,
    sb: &Superblock,
    record: &InodeRecord,
) -> DirectoryScan {
    let map = map_inode(source, sb, record);
    let Some(inode) = record.inode.as_ref() else {
        return DirectoryScan {
            map,
            blocks: Vec::new(),
            inline_parent: None,
        };
    };
    let context = DirContext::new(sb, record.ino, inode);
    let block_size = u64::from(sb.block_size());
    let mut blocks = Vec::new();
    let mut inline_parent = None;
    if inode.has_inline_data() {
        let bytes = &inode.block[..];
        inline_parent = Some(u32::from_le_bytes(bytes[..4].try_into().unwrap()));
        let mut scan = scan_dir_block(&bytes[INLINE_PARENT_BYTES..], &context);
        for entry in &mut scan.entries {
            entry.offset += INLINE_PARENT_BYTES;
        }
        blocks.push((0, Extent::new(0, bytes.len() as u64), scan));
    }
    for run in map.runs.iter().filter(|run| !run.uninitialized) {
        for step in 0..run.length {
            let location = Extent::new((run.physical + step) * block_size, block_size);
            let mut bytes = vec![0u8; block_size as usize];
            let scan = match source.read_at(location.offset, &mut bytes) {
                Ok(()) => scan_dir_block(&bytes, &context),
                Err(error) => DirBlockScan {
                    entries: Vec::new(),
                    integrity: Integrity::Failed,
                    faults: vec![(
                        0,
                        Fault {
                            kind: DamageKind::Unreadable,
                            detail: error.to_string(),
                        },
                    )],
                },
            };
            blocks.push((run.logical + step, location, scan));
        }
    }
    DirectoryScan {
        map,
        blocks,
        inline_parent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::FsFlavor;
    use crate::salvage::test_volume::Memory;
    use crate::salvage::{find_superblock, group_descriptors, read_inode};
    use std::sync::Arc;

    fn scan_of(memory: &Memory, ino: u32) -> DirectoryScan {
        let sb = find_superblock(memory).unwrap().superblock.unwrap();
        let descriptors = group_descriptors(memory, &sb, 0);
        let record = read_inode(memory, &sb, &descriptors, ino).unwrap();
        read_directory(memory, &sb, &record)
    }

    fn names(scan: &DirectoryScan, live: bool) -> Vec<String> {
        let mut names: Vec<String> = scan
            .entries()
            .filter(|e| e.live == live)
            .map(|e| String::from_utf8_lossy(&e.name).into_owned())
            .collect();
        names.sort();
        names
    }

    /// `/d` holding x1 to x5.
    fn directory(flavor: FsFlavor, mib: u64, block_size: u32) -> (Arc<Memory>, u32) {
        let memory = Memory::format(flavor, mib, block_size);
        let ino = {
            let fs = memory.mount();
            let ino = fs.apply_mkdir("/d", 0o755).unwrap();
            for i in 1..=5 {
                fs.apply_create(&format!("/d/x{i}"), 0o644).unwrap();
            }
            ino
        };
        (memory, ino)
    }

    /// Remove `name` as the format documents it: the record before it grows
    /// to cover it, and its own bytes stay where they were.
    fn absorb(memory: &Memory, ino: u32, before: &[u8], name: &[u8]) {
        let scan = scan_of(memory, ino);
        let block = scan.blocks[0].1.offset;
        let at = |n: &[u8]| scan.entries().find(|e| e.name == n).unwrap().offset as u64;
        let (prev, gone) = (at(before), at(name));
        let mut len = [0u8; 2];
        memory.copy(block + gone + 4, &mut len);
        let gone_len = u16::from_le_bytes(len);
        memory.copy(block + prev + 4, &mut len);
        let merged = u16::from_le_bytes(len) + gone_len;
        memory.poke(block + prev + 4, &merged.to_le_bytes());
    }

    #[test]
    fn live_and_deleted_entries_are_both_found() {
        let (memory, ino) = directory(FsFlavor::Ext4, 32, 2048);
        let scan = scan_of(&memory, ino);
        assert_eq!(
            names(&scan, true),
            [".", "..", "x1", "x2", "x3", "x4", "x5"]
        );
        assert_eq!(scan.blocks[0].2.integrity, Integrity::Verified);
        assert!(scan.blocks[0].2.faults.is_empty());
        absorb(&memory, ino, b"x2", b"x3");
        let scan = scan_of(&memory, ino);
        assert_eq!(names(&scan, true), [".", "..", "x1", "x2", "x4", "x5"]);
        assert_eq!(names(&scan, false), ["x3"]);
        let x3 = scan.entries().find(|e| e.name == b"x3").unwrap();
        assert_ne!(x3.inode, 0);
    }

    #[test]
    fn a_volume_without_checksums_is_unchecked() {
        let (memory, ino) = directory(FsFlavor::Ext2, 8, 1024);
        let scan = scan_of(&memory, ino);
        assert_eq!(
            names(&scan, true),
            [".", "..", "x1", "x2", "x3", "x4", "x5"]
        );
        assert_eq!(scan.blocks[0].2.integrity, Integrity::Unchecked);
    }

    #[test]
    fn a_broken_record_length_loses_only_its_own_entry() {
        let (memory, ino) = directory(FsFlavor::Ext4, 32, 2048);
        let scan = scan_of(&memory, ino);
        let x2 = scan.entries().find(|e| e.name == b"x2").unwrap().offset;
        let block = scan.blocks[0].1.offset;
        memory.poke(block + x2 as u64 + 4, &3u16.to_le_bytes());
        let scan = scan_of(&memory, ino);
        let live = names(&scan, true);
        for name in [".", "..", "x1", "x3", "x4", "x5"] {
            assert!(live.contains(&name.to_string()), "{live:?}");
        }
        let faults = &scan.blocks[0].2.faults;
        assert!(faults
            .iter()
            .any(|(at, f)| *at == x2 && f.kind == DamageKind::Malformed));
        assert!(faults.iter().any(|(_, f)| f.kind == DamageKind::Checksum));
    }

    #[test]
    fn an_entry_naming_an_inode_past_the_count_is_not_recovered_from_slack() {
        let context = DirContext {
            has_file_type: true,
            inodes_count: 100,
            csum: Checksummer {
                seed: 0,
                enabled: false,
            },
            ino: 2,
            generation: 0,
            indexed: false,
        };
        let mut block = vec![0u8; 1024];
        // One live entry covering the block, with a would-be entry in its
        // slack whose inode is past the count.
        block[0..4].copy_from_slice(&11u32.to_le_bytes());
        block[4..6].copy_from_slice(&1024u16.to_le_bytes());
        block[6] = 1;
        block[7] = 1;
        block[8] = b'a';
        block[12..16].copy_from_slice(&500u32.to_le_bytes());
        block[16..18].copy_from_slice(&12u16.to_le_bytes());
        block[18] = 1;
        block[19] = 1;
        block[20] = b'b';
        let scan = scan_dir_block(&block, &context);
        assert_eq!(scan.entries.len(), 1);
    }
}
