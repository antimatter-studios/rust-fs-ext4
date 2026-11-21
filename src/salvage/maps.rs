//! An inode's block map -- its extent tree, or its direct and indirect
//! block pointers -- followed as far as it goes, keeping every run it
//! reaches and recording where and why it stopped.
//!
//! The layouts are the format documentation's ("The Contents of
//! inode.i_block": "Direct/Indirect Block Addressing", "Extent Tree",
//! "Inline Data"). Two things a mount refuses are kept here as evidence
//! instead: an extent node whose checksum fails is still read, and a
//! pointer outside the volume ends only its own branch. A node whose magic
//! number is wrong, or a block reached twice, ends its branch too.

use super::structures::{Fault, InodeRecord};
use super::{DamageKind, Extent};
use crate::checksum::Checksummer;
use crate::inode::Inode;
use crate::superblock::Superblock;
use fs_core::BlockRead;
use std::collections::BTreeSet;

/// The extent tree's node magic number (format documentation, "Extent
/// Tree", `eh_magic`).
const EXTENT_MAGIC: u16 = 0xF30A;
/// Bytes in an extent header, and in each index or leaf entry after it.
const EXTENT_ENTRY: usize = 12;
/// An extent whose length field is above this is uninitialized, and its
/// length is the field minus this.
const EXTENT_INIT_MAX: u16 = 32768;
/// A tree deeper than this is not one the format can produce.
const EXTENT_MAX_DEPTH: u16 = 5;
/// The `i_block` area: 15 four-byte pointers, or the root of an extent
/// tree, or inline data.
const I_BLOCK_BYTES: usize = 60;
/// The direct pointers in `i_block`, before the single, double and triple
/// indirect ones.
const DIRECT_POINTERS: u64 = 12;

/// One run of logical blocks mapped to consecutive physical blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappedRun {
    /// The first logical block of the file the run holds.
    pub logical: u64,
    /// Where that block is on the volume.
    pub physical: u64,
    /// How many blocks the run covers.
    pub length: u64,
    /// The run is allocated but reads as zeros (an uninitialized extent).
    pub uninitialized: bool,
}

/// Why a map walk ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapEnd {
    /// Every branch was followed to its leaves.
    Complete,
    /// A node's header was wrong, so its branch was not followed.
    Broken,
    /// A pointer lay outside the volume, so its branch was not followed.
    OutOfRange,
    /// A block was reached twice, so the second visit was not followed.
    Cycle,
    /// The bytes of a node could not be read.
    Unreadable(Extent),
}

/// What [`map_inode`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockMap {
    /// The runs found, in logical order, adjacent runs merged.
    pub runs: Vec<MappedRun>,
    /// The blocks holding the map itself: extent tree nodes below the
    /// root, or indirect blocks.
    pub map_blocks: Vec<u64>,
    /// File contents held in the inode itself (inline data, or a fast
    /// symlink's target): the bytes in `i_block`, then for inline data past
    /// 60 bytes the value of the in-inode `system.data` attribute, up to
    /// `i_size`.
    pub inline: Option<Vec<u8>>,
    /// Where the inline bytes are on the source, in order.
    pub inline_extents: Vec<Extent>,
    /// The first reason the walk did not reach everything.
    pub end: MapEnd,
    /// Every fault seen, with where it is.
    pub faults: Vec<(Extent, Fault)>,
}

impl BlockMap {
    fn new() -> Self {
        Self {
            runs: Vec::new(),
            map_blocks: Vec::new(),
            inline: None,
            inline_extents: Vec::new(),
            end: MapEnd::Complete,
            faults: Vec::new(),
        }
    }

    fn fault(&mut self, end: MapEnd, location: Extent, kind: DamageKind, detail: String) {
        if self.end == MapEnd::Complete {
            self.end = end;
        }
        self.faults.push((location, Fault { kind, detail }));
    }

    fn push(&mut self, run: MappedRun) {
        if run.length == 0 {
            return;
        }
        self.runs.push(run);
    }

    fn finish(&mut self) {
        self.runs.sort_by_key(|run| run.logical);
        let mut merged: Vec<MappedRun> = Vec::with_capacity(self.runs.len());
        for run in self.runs.drain(..) {
            if let Some(last) = merged.last_mut() {
                if last.uninitialized == run.uninitialized
                    && last.logical + last.length == run.logical
                    && last.physical + last.length == run.physical
                {
                    last.length += run.length;
                    continue;
                }
            }
            merged.push(run);
        }
        self.runs = merged;
    }

    /// Logical blocks the runs cover, uninitialized ones included.
    pub fn mapped_blocks(&self) -> u64 {
        self.runs.iter().map(|run| run.length).sum()
    }
}

/// Where `i_block` sits in an inode.
const I_BLOCK_OFFSET: u64 = 0x28;
/// The reserved inode that maps the reserved descriptor blocks (format
/// documentation, "Special inodes": 7). Its double-indirect map names
/// those blocks in every backup group at once, so its block count is not
/// a file's and is not checked against what a file's walk reaches.
const RESIZE_INODE: u32 = 7;

/// Follow an inode's block map as far as it goes.
///
/// `record` is the inode as [`super::read_inode`] returns it, used even
/// when its checksum failed. The walk is bounded by the volume: no run or
/// node past `s_blocks_count` is accepted, and no block is followed twice.
///
/// A map can also lose blocks without any byte of it looking wrong: a
/// zeroed indirect block reads as holes, which are legal. So the blocks
/// reached -- data, map blocks and an extended-attribute block -- are
/// counted against `i_blocks`, and a map that reaches fewer than the inode
/// says it holds ends [`MapEnd::Broken`].
pub fn map_inode(source: &dyn BlockRead, sb: &Superblock, record: &InodeRecord) -> BlockMap {
    let mut map = BlockMap::new();
    let root = Extent::new(
        record.location.offset + I_BLOCK_OFFSET,
        I_BLOCK_BYTES as u64,
    );
    let Some(inode) = record.inode.as_ref() else {
        map.fault(
            MapEnd::Broken,
            record.location,
            DamageKind::Malformed,
            format!("inode {} does not parse, so it has no map", record.ino),
        );
        return map;
    };
    if inode.has_inline_data() || is_fast_symlink(inode) {
        let length = (inode.size as usize).min(I_BLOCK_BYTES);
        let mut bytes = inode.block[..length].to_vec();
        map.inline_extents
            .push(Extent::new(root.offset, length as u64));
        let rest = inode.size.saturating_sub(length as u64) as usize;
        if inode.has_inline_data() && rest > 0 {
            match system_data(&record.raw) {
                Some((at, value)) => {
                    let take = value.len().min(rest);
                    bytes.extend_from_slice(&value[..take]);
                    map.inline_extents
                        .push(Extent::new(record.location.offset + at as u64, take as u64));
                    if take < rest {
                        map.fault(
                            MapEnd::Broken,
                            record.location,
                            DamageKind::Broken,
                            format!(
                                "inode {} keeps {take} of its {rest} inline bytes past i_block",
                                record.ino
                            ),
                        );
                    }
                }
                None => map.fault(
                    MapEnd::Broken,
                    record.location,
                    DamageKind::Missing,
                    format!(
                        "inode {} holds {} bytes inline and has no system.data attribute for the {rest} past i_block",
                        record.ino, inode.size
                    ),
                ),
            }
        }
        map.inline = Some(bytes);
    } else if inode.has_extents() {
        let mut walk = ExtentWalk {
            source,
            sb,
            csum: Checksummer::from_superblock(sb),
            ino: record.ino,
            generation: inode.generation,
            visited: BTreeSet::new(),
            map: &mut map,
        };
        walk.node(&inode.block, root, None);
    } else {
        IndirectWalk {
            source,
            sb,
            visited: BTreeSet::new(),
            map: &mut map,
            wanted: wanted_blocks(sb, inode),
        }
        .root(&inode.block, root);
    }
    map.finish();
    let claimed = allocated_blocks(sb, inode).filter(|_| record.ino != RESIZE_INODE);
    if let Some(claimed) = claimed {
        let reached =
            map.mapped_blocks() + map.map_blocks.len() as u64 + u64::from(inode.file_acl != 0);
        if reached < claimed {
            map.fault(
                MapEnd::Broken,
                record.location,
                DamageKind::Broken,
                format!(
                    "inode {} holds {claimed} blocks by i_blocks, and its map reaches {reached}",
                    record.ino
                ),
            );
        }
    }
    map
}

/// The in-inode extended attribute area starts with this magic number
/// (format documentation, "Extended Attributes": `h_magic`, in the inode
/// after `i_extra_isize`).
const IBODY_XATTR_MAGIC: u32 = 0xEA02_0000;
/// The `system.` attribute name index, which `system.data` uses.
const XATTR_INDEX_SYSTEM: u8 = 7;

/// The `system.data` attribute in an inode's own attribute area: its
/// value's offset in the inode and the value. Read without trusting it:
/// an entry or value that leaves the inode ends the search.
fn system_data(raw: &[u8]) -> Option<(usize, &[u8])> {
    let extra = usize::from(u16::from_le_bytes(raw.get(0x80..0x82)?.try_into().ok()?));
    let header = 128 + extra;
    if u32::from_le_bytes(raw.get(header..header + 4)?.try_into().ok()?) != IBODY_XATTR_MAGIC {
        return None;
    }
    let first = header + 4;
    let mut at = first;
    while let Some(entry) = raw.get(at..at + 16) {
        if entry[..4] == [0, 0, 0, 0] {
            return None;
        }
        let name_len = usize::from(entry[0]);
        let index = entry[1];
        let value_offs = usize::from(u16::from_le_bytes([entry[2], entry[3]]));
        let value_size = u32::from_le_bytes(entry[8..12].try_into().ok()?) as usize;
        let name = raw.get(at + 16..at + 16 + name_len)?;
        if index == XATTR_INDEX_SYSTEM && name == b"data" {
            let start = first + value_offs;
            return Some((start, raw.get(start..start + value_size)?));
        }
        at += (16 + name_len).div_ceil(4) * 4;
    }
    None
}

/// `i_blocks` in filesystem blocks: 512-byte units, or blocks when the
/// inode is a `HUGE_FILE`. `None` on a `bigalloc` volume, where it counts
/// whole clusters and so says nothing exact about blocks.
fn allocated_blocks(sb: &Superblock, inode: &Inode) -> Option<u64> {
    if sb.feature_ro_compat & crate::features::RoCompat::BIGALLOC.bits() != 0 {
        return None;
    }
    if inode.flags & crate::inode::InodeFlags::HUGE_FILE.bits() != 0 {
        return Some(inode.blocks);
    }
    Some(inode.blocks.saturating_mul(512) / u64::from(sb.block_size()))
}

/// How many logical blocks a block-mapped inode can hold: enough for
/// `i_size`, or for `i_blocks` (512-byte units) when that is larger, so a
/// damaged size does not hide data, and never more than the volume has.
fn wanted_blocks(sb: &Superblock, inode: &Inode) -> u64 {
    let block_size = u64::from(sb.block_size());
    inode
        .size
        .div_ceil(block_size)
        .max(inode.blocks.saturating_mul(512) / block_size)
        .min(sb.blocks_count)
}

/// A symlink whose target is in `i_block` itself: under 60 bytes, and
/// mapped by neither extents nor block pointers.
fn is_fast_symlink(inode: &Inode) -> bool {
    inode.is_symlink() && !inode.has_extents() && (inode.size as usize) < I_BLOCK_BYTES
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

struct ExtentWalk<'a> {
    source: &'a dyn BlockRead,
    sb: &'a Superblock,
    csum: Checksummer,
    ino: u32,
    generation: u32,
    visited: BTreeSet<u64>,
    map: &'a mut BlockMap,
}

impl ExtentWalk<'_> {
    fn block_size(&self) -> u64 {
        u64::from(self.sb.block_size())
    }

    fn block_extent(&self, block: u64) -> Extent {
        Extent::new(block.saturating_mul(self.block_size()), self.block_size())
    }

    /// One node: the 60-byte root in the inode (`at` is `None`), or a block.
    /// `location` is where its bytes are; `depth` is what the parent says
    /// this node's depth must be.
    fn node(&mut self, bytes: &[u8], location: Extent, depth: Option<u16>) {
        let at =
            (location.length != I_BLOCK_BYTES as u64).then(|| location.offset / self.block_size());
        let place = match at {
            Some(block) => format!("extent node in block {block}"),
            None => format!("extent root in inode {}", self.ino),
        };
        let magic = u16_at(bytes, 0);
        if magic != EXTENT_MAGIC {
            self.map.fault(
                MapEnd::Broken,
                location,
                DamageKind::Signature,
                format!("{place}: magic {magic:#06x}, expected {EXTENT_MAGIC:#06x}"),
            );
            return;
        }
        let entries = usize::from(u16_at(bytes, 2));
        let max = usize::from(u16_at(bytes, 4));
        let own_depth = u16_at(bytes, 6);
        let fits = bytes.len() / EXTENT_ENTRY - 1;
        if entries > max || max > fits {
            self.map.fault(
                MapEnd::Broken,
                location,
                DamageKind::Malformed,
                format!("{place}: {entries} entries, {max} at most, room for {fits}"),
            );
        }
        let depth = match depth {
            Some(expected) if expected != own_depth => {
                self.map.fault(
                    MapEnd::Broken,
                    location,
                    DamageKind::Malformed,
                    format!("{place}: depth {own_depth}, its parent says {expected}"),
                );
                expected
            }
            _ => own_depth,
        };
        if depth > EXTENT_MAX_DEPTH {
            self.map.fault(
                MapEnd::Broken,
                location,
                DamageKind::Malformed,
                format!("{place}: depth {depth} is deeper than the format allows"),
            );
            return;
        }
        if at.is_some() && self.csum.enabled {
            // The tail follows the last entry slot (`eh_max`); on a block
            // size that is not a whole number of entries that is short of
            // the block's end, where this crate's writer has put it, so
            // either place is taken.
            let after_slots = EXTENT_ENTRY + EXTENT_ENTRY * max.min(fits) + 4;
            let verified = [after_slots, bytes.len()].iter().any(|&end| {
                end <= bytes.len()
                    && self
                        .csum
                        .verify_extent_tail(self.ino, self.generation, &bytes[..end])
            });
            if !verified {
                self.map.faults.push((
                    location,
                    Fault {
                        kind: DamageKind::Checksum,
                        detail: format!("{place}: the extent tail checksum does not match"),
                    },
                ));
            }
        }
        for index in 0..entries.min(fits) {
            let entry = &bytes[EXTENT_ENTRY * (index + 1)..EXTENT_ENTRY * (index + 2)];
            if depth == 0 {
                self.leaf(entry, location);
            } else {
                let child = (u64::from(u16_at(entry, 8)) << 32) | u64::from(u32_at(entry, 4));
                self.child(child, depth - 1, location);
            }
        }
    }

    fn leaf(&mut self, entry: &[u8], location: Extent) {
        let logical = u64::from(u32_at(entry, 0));
        let raw_length = u16_at(entry, 4);
        let physical = (u64::from(u16_at(entry, 6)) << 32) | u64::from(u32_at(entry, 8));
        let (length, uninitialized) = if raw_length > EXTENT_INIT_MAX {
            (raw_length - EXTENT_INIT_MAX, true)
        } else {
            (raw_length, false)
        };
        let length = u64::from(length);
        if physical == 0 || physical.saturating_add(length) > self.sb.blocks_count {
            self.map.fault(
                MapEnd::OutOfRange,
                location,
                DamageKind::Malformed,
                format!(
                    "extent of {length} blocks at block {physical} lies outside the {}-block volume",
                    self.sb.blocks_count
                ),
            );
            return;
        }
        self.map.push(MappedRun {
            logical,
            physical,
            length,
            uninitialized,
        });
    }

    fn child(&mut self, block: u64, depth: u16, parent: Extent) {
        if block == 0 || block >= self.sb.blocks_count {
            self.map.fault(
                MapEnd::OutOfRange,
                parent,
                DamageKind::Malformed,
                format!(
                    "extent index names block {block}, outside the {}-block volume",
                    self.sb.blocks_count
                ),
            );
            return;
        }
        if !self.visited.insert(block) {
            self.map.fault(
                MapEnd::Cycle,
                parent,
                DamageKind::Malformed,
                format!("extent node in block {block} is reached twice"),
            );
            return;
        }
        let location = self.block_extent(block);
        let mut bytes = vec![0u8; self.block_size() as usize];
        if let Err(error) = self.source.read_at(location.offset, &mut bytes) {
            self.map.fault(
                MapEnd::Unreadable(location),
                location,
                DamageKind::Unreadable,
                error.to_string(),
            );
            return;
        }
        self.map.map_blocks.push(block);
        self.node(&bytes, location, Some(depth));
    }
}

struct IndirectWalk<'a> {
    source: &'a dyn BlockRead,
    sb: &'a Superblock,
    visited: BTreeSet<u64>,
    map: &'a mut BlockMap,
    /// Logical blocks worth mapping: enough for `i_size` and `i_blocks`.
    /// Pointers past it are not followed, which bounds a garbage map.
    wanted: u64,
}

impl IndirectWalk<'_> {
    fn block_size(&self) -> u64 {
        u64::from(self.sb.block_size())
    }

    fn per_block(&self) -> u64 {
        self.block_size() / 4
    }

    fn root(&mut self, i_block: &[u8; 60], root: Extent) {
        for index in 0..DIRECT_POINTERS {
            let pointer = u64::from(u32_at(i_block, index as usize * 4));
            self.data(index, pointer, root);
        }
        let mut logical = DIRECT_POINTERS;
        for level in 1..=3u32 {
            let pointer = u64::from(u32_at(
                i_block,
                (DIRECT_POINTERS as usize + level as usize - 1) * 4,
            ));
            let span = self.per_block().pow(level);
            if logical >= self.wanted {
                return;
            }
            if pointer != 0 {
                self.indirect(pointer, level, logical, root);
            }
            logical = logical.saturating_add(span);
        }
    }

    fn data(&mut self, logical: u64, pointer: u64, location: Extent) {
        if pointer == 0 || logical >= self.wanted {
            return;
        }
        if pointer >= self.sb.blocks_count {
            self.map.fault(
                MapEnd::OutOfRange,
                location,
                DamageKind::Malformed,
                format!(
                    "block pointer {pointer} for logical block {logical} lies outside the {}-block volume",
                    self.sb.blocks_count
                ),
            );
            return;
        }
        self.map.push(MappedRun {
            logical,
            physical: pointer,
            length: 1,
            uninitialized: false,
        });
    }

    /// An indirect block of `level` (1 single, 2 double, 3 triple) whose
    /// first logical block is `first`.
    fn indirect(&mut self, block: u64, level: u32, first: u64, parent: Extent) {
        if block >= self.sb.blocks_count {
            self.map.fault(
                MapEnd::OutOfRange,
                parent,
                DamageKind::Malformed,
                format!(
                    "indirect block pointer {block} lies outside the {}-block volume",
                    self.sb.blocks_count
                ),
            );
            return;
        }
        if !self.visited.insert(block) {
            self.map.fault(
                MapEnd::Cycle,
                parent,
                DamageKind::Malformed,
                format!("indirect block {block} is reached twice"),
            );
            return;
        }
        let location = Extent::new(block * self.block_size(), self.block_size());
        let mut bytes = vec![0u8; self.block_size() as usize];
        if let Err(error) = self.source.read_at(location.offset, &mut bytes) {
            self.map.fault(
                MapEnd::Unreadable(location),
                location,
                DamageKind::Unreadable,
                error.to_string(),
            );
            return;
        }
        self.map.map_blocks.push(block);
        let span = self.per_block().pow(level - 1);
        for index in 0..self.per_block() {
            let logical = first.saturating_add(index.saturating_mul(span));
            if logical >= self.wanted {
                return;
            }
            let pointer = u64::from(u32_at(&bytes, index as usize * 4));
            if level == 1 {
                self.data(logical, pointer, location);
            } else if pointer != 0 {
                self.indirect(pointer, level - 1, logical, location);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::FsFlavor;
    use crate::salvage::test_volume::Memory;
    use crate::salvage::{find_superblock, group_descriptors, read_inode};
    use std::sync::Arc;

    fn sb_of(memory: &Memory) -> Superblock {
        find_superblock(memory).unwrap().superblock.unwrap()
    }

    fn inode_of(memory: &Memory, ino: u32) -> InodeRecord {
        let sb = sb_of(memory);
        let descriptors = group_descriptors(memory, &sb, 0);
        read_inode(memory, &sb, &descriptors, ino).unwrap()
    }

    fn map_of(memory: &Memory, ino: u32) -> BlockMap {
        map_inode(memory, &sb_of(memory), &inode_of(memory, ino))
    }

    /// The bytes the runs name, in logical order.
    fn bytes_of(memory: &Memory, map: &BlockMap, block_size: u64) -> Vec<u8> {
        let mut out = Vec::new();
        for run in &map.runs {
            let mut buf = vec![0u8; (run.length * block_size) as usize];
            memory.copy(run.physical * block_size, &mut buf);
            out.extend(buf);
        }
        out
    }

    fn pattern(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31) ^ seed)
            .collect()
    }

    /// Two files written a block at a time, alternately, so each is ten
    /// extents: more than the inode's root holds, so the tree has a leaf
    /// node below it.
    fn fragmented() -> (Arc<Memory>, u32) {
        let memory = Memory::format(FsFlavor::Ext4, 32, 2048);
        let ino = {
            let fs = memory.mount();
            let a = fs.apply_create("/a", 0o644).unwrap();
            fs.apply_create("/b", 0o644).unwrap();
            for i in 0..10u64 {
                fs.apply_pwrite("/a", i * 2048, &pattern(i as u8, 2048))
                    .unwrap();
                fs.apply_pwrite("/b", i * 2048, &pattern(0xEE, 2048))
                    .unwrap();
            }
            a
        };
        (memory, ino)
    }

    #[test]
    fn a_fragmented_file_maps_through_its_leaf_node() {
        let (memory, ino) = fragmented();
        let map = map_of(&memory, ino);
        assert_eq!(map.end, MapEnd::Complete, "{map:?}");
        assert!(map.faults.is_empty(), "{map:?}");
        assert_eq!(map.mapped_blocks(), 10);
        assert_eq!(map.map_blocks.len(), 1, "{map:?}");
        let expected: Vec<u8> = (0..10u8).flat_map(|i| pattern(i, 2048)).collect();
        assert_eq!(bytes_of(&memory, &map, 2048), expected);
    }

    #[test]
    fn a_leaf_node_with_a_wrong_magic_breaks_the_walk_there() {
        let (memory, ino) = fragmented();
        let node = map_of(&memory, ino).map_blocks[0];
        memory.poke(node * 2048, &[0, 0]);
        let map = map_of(&memory, ino);
        assert_eq!(map.end, MapEnd::Broken);
        assert_eq!(map.runs, Vec::new());
        assert_eq!(map.faults[0].1.kind, DamageKind::Signature);
    }

    #[test]
    fn a_leaf_node_whose_checksum_fails_is_still_followed() {
        let (memory, ino) = fragmented();
        let node = map_of(&memory, ino).map_blocks[0];
        memory.flip(node * 2048 + 2000); // an unused entry slot
        let map = map_of(&memory, ino);
        assert_eq!(map.end, MapEnd::Complete);
        assert_eq!(map.mapped_blocks(), 10);
        assert_eq!(map.faults.len(), 1, "{map:?}");
        assert_eq!(map.faults[0].1.kind, DamageKind::Checksum);
    }

    #[test]
    fn an_extent_outside_the_volume_is_dropped_and_the_rest_kept() {
        let (memory, ino) = fragmented();
        let node = map_of(&memory, ino).map_blocks[0];
        // The third leaf entry's physical start, low 32 bits.
        memory.poke(node * 2048 + 12 * 3 + 8, &u32::MAX.to_le_bytes());
        let map = map_of(&memory, ino);
        assert_eq!(map.end, MapEnd::OutOfRange);
        assert_eq!(map.mapped_blocks(), 9);
    }

    /// A 20 KiB file on 1 KiB-block ext2: blocks 7000 to 7011 direct,
    /// 7012 to 7019 through the single-indirect block 7100. The crate's
    /// write path does not write block-mapped files, so the map is put
    /// into the inode by hand, over blocks `mkfs` left free.
    fn block_mapped() -> (Arc<Memory>, u32, Vec<u8>) {
        let memory = Memory::format(FsFlavor::Ext2, 8, 1024);
        let ino = memory.mount().apply_create("/f", 0o644).unwrap();
        let record = inode_of(&memory, ino);
        let at = record.location.offset;
        let data = pattern(3, 20 * 1024);
        for i in 0..20u64 {
            memory.poke(
                (7000 + i) * 1024,
                &data[(i * 1024) as usize..((i + 1) * 1024) as usize],
            );
        }
        for i in 0..12u64 {
            memory.poke(at + 0x28 + i * 4, &(7000 + i as u32).to_le_bytes());
        }
        memory.poke(at + 0x28 + 12 * 4, &7100u32.to_le_bytes());
        for i in 0..8u64 {
            memory.poke(7100 * 1024 + i * 4, &(7012 + i as u32).to_le_bytes());
        }
        memory.poke(at + 0x04, &(20u32 * 1024).to_le_bytes());
        (memory, ino, data)
    }

    #[test]
    fn a_block_mapped_file_maps_through_its_indirect_block() {
        let (memory, ino, data) = block_mapped();
        let map = map_of(&memory, ino);
        assert_eq!(map.end, MapEnd::Complete, "{map:?}");
        assert_eq!(map.mapped_blocks(), 20);
        assert_eq!(map.runs.len(), 1, "contiguous runs merge: {map:?}");
        assert_eq!(map.map_blocks, [7100]);
        assert_eq!(bytes_of(&memory, &map, 1024), data);
    }

    #[test]
    fn an_indirect_pointer_outside_the_volume_keeps_the_direct_blocks() {
        let (memory, ino, _) = block_mapped();
        let record = inode_of(&memory, ino);
        memory.poke(
            record.location.offset + 0x28 + 12 * 4,
            &u32::MAX.to_le_bytes(),
        );
        let map = map_of(&memory, ino);
        assert_eq!(map.end, MapEnd::OutOfRange);
        assert_eq!(map.mapped_blocks(), 12);
    }

    #[test]
    fn an_indirect_block_that_names_itself_is_a_cycle() {
        let (memory, ino, _) = block_mapped();
        let record = inode_of(&memory, ino);
        // A double-indirect block whose first pointer is itself, and a
        // size that reaches past the single-indirect range.
        memory.poke(
            record.location.offset + 0x28 + 13 * 4,
            &7200u32.to_le_bytes(),
        );
        memory.poke(7200 * 1024, &7200u32.to_le_bytes());
        memory.poke(
            record.location.offset + 0x04,
            &(400u32 * 1024).to_le_bytes(),
        );
        let map = map_of(&memory, ino);
        assert_eq!(map.end, MapEnd::Cycle, "{map:?}");
        assert_eq!(map.mapped_blocks(), 20);
    }

    #[test]
    fn a_fast_symlink_is_inline() {
        let memory = Memory::format(FsFlavor::Ext4, 32, 2048);
        let ino = {
            let fs = memory.mount();
            fs.apply_symlink("target/of/link", "/l").unwrap()
        };
        let map = map_of(&memory, ino);
        assert_eq!(map.inline.as_deref(), Some(&b"target/of/link"[..]));
        assert!(map.runs.is_empty());
    }
}
