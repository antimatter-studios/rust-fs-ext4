//! The journal read as evidence: every copy of a filesystem block it still
//! holds, from every transaction it can find, committed or not.
//!
//! A mount replays only the transactions after `s_start`, and a clean
//! journal replays nothing. But the log is a ring that is overwritten only
//! as it wraps, so blocks checkpointed long ago are usually still in it:
//! older copies of inode tables, directory blocks, bitmaps and the
//! superblock, which are what a recovery wants when the copy in place is
//! damaged. This scan reads the whole ring, block by block, and reports
//! each descriptor's tags without trusting the journal superblock's idea of
//! where the log starts or ends.
//!
//! The layout is the format documentation's ("Journal (jbd2)": the block
//! header, descriptor tags and their flags, commit and revoke blocks, all
//! big-endian). `debugfs logdump` lists the same copies
//! (`tests/salvage_journal_oracle.rs`).

use super::maps::map_inode;
use super::structures::{group_descriptors, read_inode, Fault};
use super::{DamageKind, Extent, Integrity, SalvageError};
use crate::jbd2::{
    self, JournalSuperblock, JBD2_COMMIT_BLOCK, JBD2_DESCRIPTOR_BLOCK, JBD2_MAGIC_NUMBER,
    JBD2_REVOKE_BLOCK,
};
use crate::superblock::Superblock;
use fs_core::BlockRead;
use std::collections::BTreeSet;

/// The tag flag set when the logged block began with the journal magic,
/// whose first four bytes were then zeroed in the journal copy.
const TAG_ESCAPED: u32 = 0x1;
/// The tag flag that leaves out the 16-byte UUID after the tag.
const TAG_SAME_UUID: u32 = 0x2;
/// The tag flag on the descriptor's last tag.
const TAG_LAST: u32 = 0x8;
/// Bytes of the UUID a tag without `SAME_UUID` is followed by.
const TAG_UUID_BYTES: usize = 16;
/// The block header every journal metadata block starts with.
const HEADER_BYTES: usize = 12;

/// One copy of a filesystem block the journal holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalCopy {
    /// The filesystem block it is a copy of.
    pub fs_block: u64,
    /// The journal's own block number (0 is the journal superblock).
    pub journal_block: u64,
    /// Where the copy's bytes are on the source.
    pub location: Extent,
    /// The transaction that logged it.
    pub sequence: u32,
    /// A commit block for the transaction was found in the ring.
    pub committed: bool,
    /// The block began with the journal magic, which the copy holds as
    /// zeros: [`JournalCopy::restored`] puts it back.
    pub escaped: bool,
    /// The tag's checksum over the copy, on a journal that keeps one.
    pub integrity: Integrity,
}

impl JournalCopy {
    /// The block as it was logged: the copy's bytes, with the magic put
    /// back when the tag says it was escaped.
    pub fn restored(&self, bytes: &mut [u8]) {
        if self.escaped && bytes.len() >= 4 {
            bytes[..4].copy_from_slice(&JBD2_MAGIC_NUMBER.to_be_bytes());
        }
    }
}

/// What the journal holds.
#[derive(Clone, Debug)]
pub struct JournalScan {
    /// The journal superblock, when it parses.
    pub superblock: Option<JournalSuperblock>,
    /// Where it is: journal block 0.
    pub superblock_location: Option<Extent>,
    /// Every copy found, in ring order.
    pub copies: Vec<JournalCopy>,
    /// Filesystem blocks a revoke record names, with the revoking
    /// transaction: their earlier copies are not to be replayed.
    pub revoked: Vec<(u64, u32)>,
    /// What stopped parts of the scan.
    pub faults: Vec<(Extent, Fault)>,
}

impl JournalScan {
    /// The copies of filesystem block `block`, newest transaction first.
    pub fn copies_of(&self, block: u64) -> Vec<&JournalCopy> {
        let mut found: Vec<&JournalCopy> =
            self.copies.iter().filter(|c| c.fs_block == block).collect();
        found.sort_by_key(|c| std::cmp::Reverse(c.sequence));
        found
    }
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn be16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(bytes[at..at + 2].try_into().unwrap())
}

/// Scan the volume's internal journal. `Ok` with no superblock and no
/// copies when the volume has no journal inode; an external journal
/// device is reported as a fault.
pub fn scan_journal(source: &dyn BlockRead, sb: &Superblock) -> Result<JournalScan, SalvageError> {
    let mut scan = JournalScan {
        superblock: None,
        superblock_location: None,
        copies: Vec::new(),
        revoked: Vec::new(),
        faults: Vec::new(),
    };
    if sb.feature_incompat & crate::features::Incompat::JOURNAL_DEV.bits() != 0 {
        scan.faults.push((
            Extent::new(0, 0),
            Fault {
                kind: DamageKind::Missing,
                detail: "the journal is on an external device".to_string(),
            },
        ));
        return Ok(scan);
    }
    if sb.journal_inode == 0 {
        return Ok(scan);
    }
    let descriptors = group_descriptors(source, sb, 0);
    let record = read_inode(source, sb, &descriptors, sb.journal_inode)?;
    let map = map_inode(source, sb, &record);
    scan.faults.extend(map.faults.iter().cloned());
    let block_size = u64::from(sb.block_size());
    // Journal block to source byte offset.
    let place = |journal_block: u64| -> Option<Extent> {
        map.runs
            .iter()
            .find(|run| (run.logical..run.logical + run.length).contains(&journal_block))
            .map(|run| {
                Extent::new(
                    (run.physical + journal_block - run.logical) * block_size,
                    block_size,
                )
            })
    };
    let read = |location: Extent| -> Option<Vec<u8>> {
        let mut bytes = vec![0u8; location.length as usize];
        source
            .read_at(location.offset, &mut bytes)
            .ok()
            .map(|_| bytes)
    };
    let Some(sb_location) = place(0) else {
        scan.faults.push((
            record.location,
            Fault {
                kind: DamageKind::Missing,
                detail: "the journal inode maps no block 0".to_string(),
            },
        ));
        return Ok(scan);
    };
    scan.superblock_location = Some(sb_location);
    let jsb = match read(sb_location).map(|bytes| JournalSuperblock::parse(&bytes)) {
        Some(Ok(jsb)) => jsb,
        Some(Err(error)) => {
            scan.faults.push((
                sb_location,
                Fault {
                    kind: DamageKind::Signature,
                    detail: error.to_string(),
                },
            ));
            return Ok(scan);
        }
        None => {
            scan.faults.push((
                sb_location,
                Fault {
                    kind: DamageKind::Unreadable,
                    detail: "the journal superblock could not be read".to_string(),
                },
            ));
            return Ok(scan);
        }
    };
    let seed = jsb.uses_csum_v2_or_v3().then(|| jsb.csum_seed());
    let tail = if seed.is_some() {
        jbd2::BLOCK_TAIL_BYTES
    } else {
        0
    };
    let tag_bytes = jsb.tag_bytes();
    let v3 = jsb.uses_csum_v3();
    let bit64 = jsb.uses_64bit();
    let last = map
        .runs
        .iter()
        .map(|run| run.logical + run.length)
        .max()
        .unwrap_or(0);
    let ring_end = u64::from(jsb.max_len).min(last).max(1);
    let first = u64::from(jsb.first.max(1));
    let capacity = ring_end.saturating_sub(first).max(1);
    let wrap = |block: u64| first + (block - first) % capacity;

    let mut commits = BTreeSet::new();
    let mut found = Vec::new();
    let mut journal_block = first;
    while journal_block < ring_end {
        let Some(location) = place(journal_block) else {
            journal_block += 1;
            continue;
        };
        let Some(bytes) = read(location) else {
            scan.faults.push((
                location,
                Fault {
                    kind: DamageKind::Unreadable,
                    detail: format!("journal block {journal_block} could not be read"),
                },
            ));
            journal_block += 1;
            continue;
        };
        journal_block += 1;
        if bytes.len() < HEADER_BYTES || be32(&bytes, 0) != JBD2_MAGIC_NUMBER {
            continue;
        }
        let kind = be32(&bytes, 4);
        let sequence = be32(&bytes, 8);
        match kind {
            JBD2_COMMIT_BLOCK => {
                commits.insert(sequence);
            }
            JBD2_REVOKE_BLOCK => {
                let count = (be32(&bytes, HEADER_BYTES) as usize).min(bytes.len() - tail);
                let width = if bit64 { 8 } else { 4 };
                let mut at = HEADER_BYTES + 4;
                while at + width <= count {
                    let block = if bit64 {
                        u64::from(be32(&bytes, at)) << 32 | u64::from(be32(&bytes, at + 4))
                    } else {
                        u64::from(be32(&bytes, at))
                    };
                    scan.revoked.push((block, sequence));
                    at += width;
                }
            }
            JBD2_DESCRIPTOR_BLOCK => {
                let mut at = HEADER_BYTES;
                let mut data = journal_block - 1;
                while at + tag_bytes <= bytes.len() - tail {
                    let (low, flags, high, stored) = if v3 {
                        (
                            be32(&bytes, at),
                            be32(&bytes, at + 4),
                            be32(&bytes, at + 8),
                            Some(be32(&bytes, at + 12)),
                        )
                    } else {
                        let high = if bit64 { be32(&bytes, at + 8) } else { 0 };
                        let stored = (tag_bytes == 10 || tag_bytes == 14)
                            .then(|| u32::from(be16(&bytes, at + 4)));
                        (
                            be32(&bytes, at),
                            u32::from(be16(&bytes, at + 6)),
                            high,
                            stored,
                        )
                    };
                    at += tag_bytes;
                    if flags & TAG_SAME_UUID == 0 {
                        at += TAG_UUID_BYTES;
                    }
                    data = wrap(data + 1);
                    let fs_block = if bit64 {
                        u64::from(high) << 32 | u64::from(low)
                    } else {
                        u64::from(low)
                    };
                    if let Some(copy_location) = place(data) {
                        let integrity = match (seed, stored) {
                            (Some(seed), Some(stored)) => match read(copy_location) {
                                Some(copy) => {
                                    let computed = jbd2::tag_checksum(seed, sequence, &copy);
                                    let computed = if v3 { computed } else { computed & 0xFFFF };
                                    if computed == stored {
                                        Integrity::Verified
                                    } else {
                                        Integrity::Failed
                                    }
                                }
                                None => Integrity::Failed,
                            },
                            _ => Integrity::Unchecked,
                        };
                        found.push(JournalCopy {
                            fs_block,
                            journal_block: data,
                            location: copy_location,
                            sequence,
                            committed: false,
                            escaped: flags & TAG_ESCAPED != 0,
                            integrity,
                        });
                    }
                    if flags & TAG_LAST != 0 {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    // A descriptor from an earlier pass round the ring can name a journal
    // block a later transaction has since written. The newest claim on a
    // block is the one its bytes belong to, and a block holding a journal
    // header is no one's copy: a logged block that began with the magic was
    // escaped.
    let mut newest: std::collections::BTreeMap<u64, u32> = std::collections::BTreeMap::new();
    for copy in &found {
        let entry = newest.entry(copy.journal_block).or_insert(copy.sequence);
        if sequence_after(copy.sequence, *entry) {
            *entry = copy.sequence;
        }
    }
    found.retain(|copy| {
        newest.get(&copy.journal_block) == Some(&copy.sequence)
            && read(copy.location).is_some_and(|b| be32(&b, 0) != JBD2_MAGIC_NUMBER)
    });
    for copy in &mut found {
        copy.committed = commits.contains(&copy.sequence);
    }
    scan.copies = found;
    scan.superblock = Some(jsb);
    Ok(scan)
}

/// Whether transaction `a` is later than `b`, with sequence numbers that
/// wrap round 2^32.
fn sequence_after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::FsFlavor;
    use crate::salvage::find_superblock;
    use crate::salvage::test_volume::Memory;

    #[test]
    fn a_volume_without_a_journal_has_nothing_to_scan() {
        let memory = Memory::format(FsFlavor::Ext4, 32, 2048);
        let sb = find_superblock(&*memory).unwrap().superblock.unwrap();
        let scan = scan_journal(&*memory, &sb).unwrap();
        assert!(scan.superblock.is_none());
        assert!(scan.copies.is_empty());
        assert!(scan.faults.is_empty());
    }

    #[test]
    fn the_crates_own_journalled_writes_are_found_committed() {
        let memory = Memory::format(FsFlavor::Ext3, 8, 1024);
        {
            let fs = memory.mount();
            fs.apply_mkdir("/d", 0o755).unwrap();
            fs.apply_create("/d/f", 0o644).unwrap();
        }
        let sb = find_superblock(&*memory).unwrap().superblock.unwrap();
        let scan = scan_journal(&*memory, &sb).unwrap();
        assert!(scan.superblock.is_some(), "{:?}", scan.faults);
        assert!(!scan.copies.is_empty());
        assert!(scan.copies.iter().all(|c| c.committed), "{:?}", scan.copies);
    }

    #[test]
    fn a_journal_superblock_with_a_wrong_magic_is_a_fault() {
        let memory = Memory::format(FsFlavor::Ext3, 8, 1024);
        let sb = find_superblock(&*memory).unwrap().superblock.unwrap();
        let at = scan_journal(&*memory, &sb)
            .unwrap()
            .superblock_location
            .unwrap();
        memory.poke(at.offset, &[0, 0, 0, 0]);
        let scan = scan_journal(&*memory, &sb).unwrap();
        assert!(scan.superblock.is_none());
        assert_eq!(scan.faults[0].1.kind, DamageKind::Signature);
    }

    #[test]
    fn sequence_numbers_compare_across_the_wrap() {
        assert!(sequence_after(1, u32::MAX));
        assert!(!sequence_after(u32::MAX, 1));
        assert!(sequence_after(7, 6));
    }
}
