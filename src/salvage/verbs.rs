//! The salvage verbs: find candidates, walk extents, rebuild the tree,
//! report damage, read by extent and materialize from a plan.
//!
//! These are the family's salvage convention in ext4's terms. A candidate
//! is an inode found in an inode table, live or deleted, or a superblock
//! copy; a walk is an inode's block map as source byte ranges; the tree is
//! the parent claims directory entries make; a plan is a list of source
//! extents, zeros and literal bytes that rebuilds a file.
//!
//! [`Ext4Salvage`] holds the volume's geometry and nothing else. Every
//! method takes the source as a [`fs_core::BlockRead`], so the verbs can be
//! run over any copy of the volume, and none can write to it.

use super::dirs::read_directory;
use super::maps::{map_inode, MapEnd};
use super::structures::{
    find_superblock, group_descriptors, read_inode_at, structure_damage, superblock_copies,
    DescriptorRecord, InodeRecord,
};
use super::{Damage, DamageKind, Extent, Integrity, SalvageError, Severity};
use crate::bgd::BgdFlags;
use crate::inode::{S_IFDIR, S_IFMT, S_IFREG};
use crate::superblock::Superblock;
use fs_core::BlockRead;
use std::collections::BTreeMap;
use std::io;
use std::ops::ControlFlow;

/// The root directory's inode number.
const ROOT_INO: u32 = 2;
/// How much a plan step reads or writes at a time.
const CHUNK: u64 = 1 << 20;

/// Where a scan or a report sends what it finds, one at a time. Breaking
/// stops the work; the report says where, so it can resume.
pub trait Sink<T> {
    fn accept(&mut self, item: T) -> ControlFlow<()>;
}

impl<T> Sink<T> for Vec<T> {
    fn accept(&mut self, item: T) -> ControlFlow<()> {
        self.push(item);
        ControlFlow::Continue(())
    }
}

impl<T, F: FnMut(T) -> ControlFlow<()>> Sink<T> for F {
    fn accept(&mut self, item: T) -> ControlFlow<()> {
        self(item)
    }
}

/// What a candidate is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CandidateKind {
    File,
    Directory,
    /// A superblock copy, or one of the reserved inodes the format keeps
    /// for itself (the journal, the resize inode).
    Structure,
    /// A symlink, device, pipe or socket.
    Other,
}

/// Whether the format records the candidate as in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CandidateState {
    /// Linked and not marked deleted.
    Live,
    /// No links, or a deletion time set.
    Deleted,
    /// The bytes do not say.
    Unknown,
}

/// What an inode candidate says about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InodeDetail {
    pub ino: u32,
    pub mode: u16,
    pub links: u16,
    /// `i_dtime`: when the inode was deleted, or 0.
    pub dtime: i64,
    pub mtime: i64,
    pub flags: u32,
    pub generation: u32,
}

/// The format-specific part of a candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CandidateDetail {
    Inode(InodeDetail),
    /// A superblock copy, held by `group`.
    Superblock {
        group: u64,
    },
}

/// One possible object, with the evidence for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The bytes that describe it: the inode, or the superblock copy.
    pub location: Extent,
    pub kind: CandidateKind,
    pub state: CandidateState,
    pub integrity: Integrity,
    /// ext4 keeps names in directories, not inodes; see [`Tree`].
    pub name: Option<String>,
    /// `i_size`, for an inode.
    pub size: Option<u64>,
    pub detail: CandidateDetail,
}

impl Candidate {
    /// The inode number, for an inode candidate.
    pub fn ino(&self) -> Option<u32> {
        match &self.detail {
            CandidateDetail::Inode(detail) => Some(detail.ino),
            CandidateDetail::Superblock { .. } => None,
        }
    }
}

/// The outcome of a candidate scan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Candidates passed to the sink.
    pub candidates: u64,
    /// Inode table ranges inside the scan that could not be read.
    pub unreadable: Vec<Extent>,
    /// Set when the sink stopped the scan: the first byte not yet scanned.
    pub stopped_at: Option<u64>,
}

/// Why a walk ended; the same answers as a block map's.
pub type WalkEnd = MapEnd;

/// One stretch of a file, in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    /// Bytes stored at this source extent.
    Stored(Extent),
    /// This many bytes that read as zeros: a hole, an uninitialized
    /// extent, or the part of the file the map no longer reaches.
    Zeros(u64),
    /// Bytes kept in the inode itself.
    Inline(Vec<u8>),
}

impl Piece {
    pub fn length(&self) -> u64 {
        match self {
            Self::Stored(extent) => extent.length,
            Self::Zeros(length) => *length,
            Self::Inline(bytes) => bytes.len() as u64,
        }
    }
}

/// An inode's storage claims, followed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtentWalk {
    /// Source byte ranges in file order, merged where adjacent and trimmed
    /// to the claimed size. A broken walk keeps every extent it reached.
    pub extents: Vec<Extent>,
    /// Bytes the inode claims: `i_size`.
    pub claimed: u64,
    pub end: WalkEnd,
    /// The whole file in order, holes and inline bytes included, so a plan
    /// can put every stored byte back at its own offset.
    pub pieces: Vec<Piece>,
}

impl ExtentWalk {
    /// A plan that rebuilds the file: every stored extent at its own
    /// offset, zeros between, `claimed` bytes long.
    pub fn plan(&self) -> Plan {
        Plan {
            steps: self
                .pieces
                .iter()
                .map(|piece| match piece {
                    Piece::Stored(extent) => PlanStep::Extent(*extent),
                    Piece::Zeros(length) => PlanStep::Zeros(*length),
                    Piece::Inline(bytes) => PlanStep::Bytes(bytes.clone()),
                })
                .collect(),
            length: Some(self.claimed),
            valid_length: None,
        }
    }
}

/// A parent a candidate is claimed to have.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Parent {
    /// The volume's root: inode 2 is its own parent.
    Root,
    /// The parent candidate's `location.offset`.
    Candidate(u64),
}

/// One parent claim, and the name it gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParentClaim {
    pub parent: Parent,
    /// The entry's name in the parent; empty for a claim made by the
    /// child's own `..` entry, which names no one.
    pub name: Vec<u8>,
    /// The entry is live in a live directory. A deleted entry, or any
    /// entry of a deleted directory, is weaker evidence.
    pub live: bool,
}

/// Every parent claim found for one candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeNode {
    /// The candidate's `location.offset`.
    pub candidate: u64,
    pub ino: u32,
    /// None makes an orphan; several conflict. Claims are evidence and may
    /// form cycles.
    pub parents: Vec<ParentClaim>,
}

/// The relationships the directories claim.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    pub nodes: Vec<TreeNode>,
}

impl Tree {
    /// The node for the candidate at `candidate`.
    pub fn node(&self, candidate: u64) -> Option<&TreeNode> {
        self.nodes.iter().find(|node| node.candidate == candidate)
    }

    /// Candidates no directory claims.
    pub fn orphans(&self) -> impl Iterator<Item = &TreeNode> {
        self.nodes.iter().filter(|node| node.parents.is_empty())
    }
}

/// The outcome of a damage report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DamageReport {
    /// Findings passed to the sink.
    pub reported: u64,
    /// Set when the sink stopped the report before every check ran.
    pub stopped: bool,
}

/// One step of a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanStep {
    /// Copy these source bytes.
    Extent(Extent),
    /// Emit this many zero bytes.
    Zeros(u64),
    /// Emit these bytes verbatim.
    Bytes(Vec<u8>),
    /// Copy `count` whole blocks, numbered from `first`.
    Clusters { first: u64, count: u64 },
}

/// What to produce, and from what.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<PlanStep>,
    /// The output's final length: the steps' output is cut or padded with
    /// zeros to it.
    pub length: Option<u64>,
    /// Output bytes from this offset on are zeros, whatever the steps say.
    pub valid_length: Option<u64>,
}

/// Where some output bytes came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializedRange {
    pub output: Extent,
    /// The source bytes; `None` for zeros and literal bytes.
    pub source: Option<Extent>,
    /// False when the source bytes could not be read: the output holds
    /// zeros there, and this flag, not the bytes, is the evidence.
    pub readable: bool,
}

/// The provenance of every output byte.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MaterializeReport {
    pub ranges: Vec<MaterializedRange>,
    pub written: u64,
}

impl MaterializeReport {
    fn push(&mut self, length: u64, source: Option<Extent>, readable: bool) {
        if length == 0 {
            return;
        }
        let output = Extent::new(self.written, length);
        self.written += length;
        if let Some(last) = self.ranges.last_mut() {
            let joins = last.readable == readable
                && match (last.source, source) {
                    (None, None) => true,
                    (Some(a), Some(b)) => a.end() == Some(b.offset),
                    _ => false,
                };
            if joins {
                last.output.length += length;
                if let (Some(a), Some(b)) = (last.source.as_mut(), source) {
                    a.length += b.length;
                }
                return;
            }
        }
        self.ranges.push(MaterializedRange {
            output,
            source,
            readable,
        });
    }

    /// Output bytes whose source could not be read.
    pub fn unreadable(&self) -> u64 {
        self.ranges
            .iter()
            .filter(|range| !range.readable)
            .map(|range| range.output.length)
            .sum()
    }
}

/// A verb the adapter does not offer. Empty for ext4: every verb is here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Feature {
    FindCandidates,
    WalkExtents,
    RebuildTree,
    ReportDamage,
    ReadExtent,
    Materialize,
    DeletedCandidates,
    DeletedExtents,
    ClusterSteps,
}

/// The salvage verbs for one ext4 volume's geometry.
#[derive(Clone, Debug)]
pub struct Ext4Salvage {
    sb: Superblock,
}

impl Ext4Salvage {
    /// The geometry from the first sound superblock copy on `source`.
    pub fn new(source: &dyn BlockRead) -> Result<Self, SalvageError> {
        let chosen = find_superblock(source)?;
        Ok(Self::from_superblock(
            chosen.superblock.ok_or(SalvageError::NoGeometry)?,
        ))
    }

    /// The geometry of a superblock the caller chose.
    pub fn from_superblock(sb: Superblock) -> Self {
        Self { sb }
    }

    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    /// What this adapter lacks: nothing. Deleted inodes are candidates;
    /// what their maps still claim is walked like any other map, which is
    /// often nothing, because deleting a file clears its map.
    pub fn missing(&self) -> Vec<Feature> {
        Vec::new()
    }

    fn block_size(&self) -> u64 {
        u64::from(self.sb.block_size())
    }

    /// The descriptor to locate `group`'s inode table through: the primary,
    /// unless it is unusable and a backup is not.
    fn descriptor_for(tables: &[Vec<DescriptorRecord>], group: u64) -> Option<&DescriptorRecord> {
        let usable = |record: &&DescriptorRecord| {
            record.descriptor.is_some()
                && !matches!(
                    record.fault.as_ref().map(|f| f.kind),
                    Some(DamageKind::Malformed | DamageKind::Unreadable)
                )
        };
        tables
            .iter()
            .filter_map(|table| table.iter().find(|record| record.group == group))
            .find(usable)
    }

    /// Find candidates whose describing bytes lie in `range`: superblock
    /// copies, and every inode table slot that is not all zeros. An inode
    /// table a group's descriptor marks uninitialized is skipped, unless
    /// that descriptor's own checksum failed.
    pub fn find_candidates(
        &self,
        source: &dyn BlockRead,
        range: Extent,
        sink: &mut dyn Sink<Candidate>,
    ) -> Result<ScanReport, SalvageError> {
        let mut report = ScanReport::default();
        let inside = |extent: Extent| {
            extent.offset >= range.offset
                && extent.end().zip(range.end()).is_some_and(|(a, b)| a <= b)
        };
        for copy in superblock_copies(source, &self.sb) {
            if !inside(copy.location) || copy.superblock.is_none() {
                continue;
            }
            report.candidates += 1;
            let candidate = Candidate {
                location: copy.location,
                kind: CandidateKind::Structure,
                state: CandidateState::Live,
                integrity: copy.integrity,
                name: None,
                size: None,
                detail: CandidateDetail::Superblock { group: copy.group },
            };
            if sink.accept(candidate).is_break() {
                report.stopped_at = Some(copy.location.end().unwrap_or(u64::MAX));
                return Ok(report);
            }
        }
        let tables: Vec<Vec<DescriptorRecord>> = (0..super::descriptor_table_copies(&self.sb))
            .map(|copy| group_descriptors(source, &self.sb, copy))
            .collect();
        let inode_size = u64::from(self.sb.inode_size);
        let per_group = u64::from(self.sb.inodes_per_group);
        let block_size = self.block_size();
        let inodes_per_block = (block_size / inode_size).max(1);
        for group in 0..self.sb.block_group_count() {
            let Some(record) = Self::descriptor_for(&tables, group) else {
                continue;
            };
            let descriptor = record
                .descriptor
                .as_ref()
                .expect("usable descriptors parse");
            if descriptor.flags().contains(BgdFlags::INODE_UNINIT)
                && record.integrity != Integrity::Failed
            {
                continue;
            }
            let table = descriptor.inode_table.saturating_mul(block_size);
            let mut index = 0;
            while index < per_group {
                let count = inodes_per_block.min(per_group - index);
                let chunk = Extent::new(table + index * inode_size, count * inode_size);
                index += count;
                let overlaps = chunk.offset < range.end().unwrap_or(u64::MAX)
                    && chunk.end().unwrap_or(u64::MAX) > range.offset;
                if !overlaps {
                    continue;
                }
                let mut bytes = vec![0u8; chunk.length as usize];
                if source.read_at(chunk.offset, &mut bytes).is_err() {
                    report.unreadable.push(chunk);
                    continue;
                }
                for slot in 0..count {
                    let at = (slot * inode_size) as usize;
                    let raw = &bytes[at..at + inode_size as usize];
                    let location = Extent::new(chunk.offset + slot * inode_size, inode_size);
                    if raw.iter().all(|&b| b == 0) || !inside(location) {
                        continue;
                    }
                    let ino = (group * per_group + (index - count) + slot + 1) as u32;
                    let mut record = InodeRecord {
                        ino,
                        location,
                        integrity: Integrity::Failed,
                        inode: None,
                        raw: Vec::new(),
                        uninitialized: false,
                        fault: None,
                    };
                    super::structures::judge_inode(&self.sb, &mut record, raw.to_vec());
                    let Some(candidate) = self.inode_candidate(&record) else {
                        continue;
                    };
                    report.candidates += 1;
                    if sink.accept(candidate).is_break() {
                        report.stopped_at = Some(location.end().unwrap_or(u64::MAX));
                        return Ok(report);
                    }
                }
            }
        }
        Ok(report)
    }

    /// The candidate an inode record makes, or `None` for bytes that do not
    /// parse as an inode with a type.
    fn inode_candidate(&self, record: &InodeRecord) -> Option<Candidate> {
        let inode = record.inode.as_ref()?;
        if inode.mode == 0 {
            return None;
        }
        let reserved = record.ino < self.sb.first_inode && record.ino != ROOT_INO;
        let kind = match inode.mode & S_IFMT {
            _ if reserved => CandidateKind::Structure,
            S_IFREG => CandidateKind::File,
            S_IFDIR => CandidateKind::Directory,
            _ => CandidateKind::Other,
        };
        let state = if inode.links_count > 0 && inode.dtime == 0 {
            CandidateState::Live
        } else {
            CandidateState::Deleted
        };
        Some(Candidate {
            location: record.location,
            kind,
            state,
            integrity: record.integrity,
            name: None,
            size: Some(inode.size),
            detail: CandidateDetail::Inode(InodeDetail {
                ino: record.ino,
                mode: inode.mode,
                links: inode.links_count,
                dtime: inode.dtime,
                mtime: inode.mtime,
                flags: inode.flags,
                generation: inode.generation,
            }),
        })
    }

    /// The inode a candidate's bytes hold, read again from the source.
    fn record_of(
        &self,
        source: &dyn BlockRead,
        candidate: &Candidate,
    ) -> Result<InodeRecord, SalvageError> {
        let ino = candidate.ino().ok_or_else(|| {
            SalvageError::OutOfRange("a superblock candidate has no storage to walk".to_string())
        })?;
        Ok(read_inode_at(
            source,
            &self.sb,
            ino,
            candidate.location.offset,
        ))
    }

    /// Follow a candidate's storage claims: its block map as source byte
    /// ranges, in file order, cut to `i_size`.
    pub fn walk_extents(
        &self,
        source: &dyn BlockRead,
        candidate: &Candidate,
    ) -> Result<ExtentWalk, SalvageError> {
        let record = self.record_of(source, candidate)?;
        let map = map_inode(source, &self.sb, &record);
        let claimed = record.inode.as_ref().map_or(0, |inode| inode.size);
        let mut pieces = Vec::new();
        if let Some(bytes) = &map.inline {
            pieces.push(Piece::Inline(bytes.clone()));
        } else {
            let block_size = self.block_size();
            let mut at = 0u64;
            for run in &map.runs {
                let start = run.logical.saturating_mul(block_size);
                if start >= claimed {
                    break;
                }
                if start > at {
                    pieces.push(Piece::Zeros(start - at));
                }
                let length = run.length.saturating_mul(block_size).min(claimed - start);
                pieces.push(if run.uninitialized {
                    Piece::Zeros(length)
                } else {
                    Piece::Stored(Extent::new(run.physical * block_size, length))
                });
                at = start + length;
            }
            if at < claimed {
                pieces.push(Piece::Zeros(claimed - at));
            }
        }
        let mut extents: Vec<Extent> = Vec::new();
        for piece in &pieces {
            if let Piece::Stored(extent) = piece {
                match extents.last_mut() {
                    Some(last) if last.end() == Some(extent.offset) => last.length += extent.length,
                    _ => extents.push(*extent),
                }
            }
        }
        Ok(ExtentWalk {
            extents,
            claimed,
            end: map.end,
            pieces,
        })
    }

    /// Every parent claim the directories among `candidates` make, for
    /// every inode candidate. A directory's entries claim their children;
    /// its own `..` entry claims its parent. The root claims itself.
    pub fn rebuild_tree(
        &self,
        source: &dyn BlockRead,
        candidates: &[Candidate],
    ) -> Result<Tree, SalvageError> {
        let by_ino: BTreeMap<u32, &Candidate> = candidates
            .iter()
            .filter_map(|candidate| Some((candidate.ino()?, candidate)))
            .collect();
        let mut claims: BTreeMap<u32, Vec<ParentClaim>> =
            by_ino.keys().map(|&ino| (ino, Vec::new())).collect();
        let mut add = |child: u32, claim: ParentClaim| {
            if let Some(list) = claims.get_mut(&child) {
                let same = list.iter_mut().find(|c| {
                    c.parent == claim.parent
                        && (c.name.is_empty() || claim.name.is_empty() || c.name == claim.name)
                });
                match same {
                    Some(existing) => {
                        if existing.name.is_empty() {
                            existing.name = claim.name;
                        }
                        existing.live |= claim.live;
                    }
                    None => list.push(claim),
                }
            }
        };
        for (&ino, candidate) in &by_ino {
            if ino == ROOT_INO {
                add(
                    ino,
                    ParentClaim {
                        parent: Parent::Root,
                        name: Vec::new(),
                        live: true,
                    },
                );
            }
            if candidate.kind != CandidateKind::Directory {
                continue;
            }
            let record = self.record_of(source, candidate)?;
            let scan = read_directory(source, &self.sb, &record);
            let dir_live = candidate.state == CandidateState::Live;
            let here = Parent::Candidate(candidate.location.offset);
            let parent_of = |parent: u32| {
                if parent == ROOT_INO && ino == ROOT_INO {
                    Some(Parent::Root)
                } else {
                    by_ino
                        .get(&parent)
                        .map(|c| Parent::Candidate(c.location.offset))
                }
            };
            if let Some(parent) = scan.inline_parent.and_then(parent_of) {
                if ino != ROOT_INO {
                    add(
                        ino,
                        ParentClaim {
                            parent,
                            name: Vec::new(),
                            live: dir_live,
                        },
                    );
                }
            }
            for entry in scan.entries() {
                match entry.name.as_slice() {
                    b"." => {}
                    b".." => {
                        if ino != ROOT_INO && entry.live {
                            if let Some(parent) = parent_of(entry.inode) {
                                add(
                                    ino,
                                    ParentClaim {
                                        parent,
                                        name: Vec::new(),
                                        live: dir_live,
                                    },
                                );
                            }
                        }
                    }
                    name => add(
                        entry.inode,
                        ParentClaim {
                            parent: here,
                            name: name.to_vec(),
                            live: entry.live && dir_live,
                        },
                    ),
                }
            }
        }
        Ok(Tree {
            nodes: by_ino
                .iter()
                .map(|(&ino, candidate)| TreeNode {
                    candidate: candidate.location.offset,
                    ino,
                    parents: claims.remove(&ino).unwrap_or_default(),
                })
                .collect(),
        })
    }

    /// Everything observed to be wrong: the superblock copies and
    /// descriptor tables, then each live inode -- its checksum, its map,
    /// and for a directory its blocks.
    pub fn report_damage(
        &self,
        source: &dyn BlockRead,
        sink: &mut dyn Sink<Damage>,
    ) -> Result<DamageReport, SalvageError> {
        let mut report = DamageReport::default();
        let mut send = |damage: Damage, report: &mut DamageReport| {
            report.reported += 1;
            sink.accept(damage).is_break()
        };
        for damage in structure_damage(source)? {
            if send(damage, &mut report) {
                report.stopped = true;
                return Ok(report);
            }
        }
        let mut candidates = Vec::new();
        let whole = Extent::new(0, source.size_bytes());
        let scan = self.find_candidates(source, whole, &mut candidates)?;
        for extent in scan.unreadable {
            let damage = Damage {
                kind: DamageKind::Unreadable,
                severity: Severity::Error,
                location: extent,
                structure: "inode table".to_string(),
                detail: "these inode table bytes could not be read".to_string(),
            };
            if send(damage, &mut report) {
                report.stopped = true;
                return Ok(report);
            }
        }
        for candidate in candidates
            .iter()
            .filter(|c| c.state == CandidateState::Live && c.ino().is_some())
        {
            for damage in self.inode_damage(source, candidate)? {
                if send(damage, &mut report) {
                    report.stopped = true;
                    return Ok(report);
                }
            }
        }
        Ok(report)
    }

    fn inode_damage(
        &self,
        source: &dyn BlockRead,
        candidate: &Candidate,
    ) -> Result<Vec<Damage>, SalvageError> {
        let record = self.record_of(source, candidate)?;
        let ino = record.ino;
        let mut found = Vec::new();
        if let Some(fault) = &record.fault {
            found.push(Damage {
                kind: fault.kind,
                severity: Severity::Error,
                location: record.location,
                structure: format!("inode {ino}"),
                detail: fault.detail.clone(),
            });
        }
        let map = if candidate.kind == CandidateKind::Directory {
            let scan = read_directory(source, &self.sb, &record);
            for (logical, at, block) in &scan.blocks {
                for (offset, fault) in &block.faults {
                    found.push(Damage {
                        kind: fault.kind,
                        severity: Severity::Error,
                        location: Extent::new(at.offset + *offset as u64, 0),
                        structure: format!("directory inode {ino}, block {logical}"),
                        detail: fault.detail.clone(),
                    });
                }
            }
            scan.map
        } else {
            map_inode(source, &self.sb, &record)
        };
        for (location, fault) in map.faults {
            found.push(Damage {
                kind: fault.kind,
                severity: Severity::Error,
                location,
                structure: format!("block map of inode {ino}"),
                detail: fault.detail,
            });
        }
        Ok(found)
    }

    /// Read the bytes of `extent` from `offset` within it into `output`.
    /// Returns how many were read: fewer than asked only at the extent's end.
    pub fn read_extent(
        &self,
        source: &dyn BlockRead,
        extent: Extent,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, SalvageError> {
        extent
            .end()
            .filter(|&end| end <= source.size_bytes())
            .ok_or_else(|| {
                SalvageError::OutOfRange(format!(
                    "{} byte(s) at {} lie outside the {}-byte source",
                    extent.length,
                    extent.offset,
                    source.size_bytes()
                ))
            })?;
        if offset >= extent.length {
            return Ok(0);
        }
        let count = (extent.length - offset).min(output.len() as u64) as usize;
        source
            .read_at(extent.offset + offset, &mut output[..count])
            .map_err(|error| SalvageError::Read(error.to_string()))?;
        Ok(count)
    }

    /// The source bytes of `count` blocks from block `first`: ext4's
    /// allocation units are its blocks, numbered from the volume's start.
    pub fn blocks(&self, first: u64, count: u64) -> Option<Extent> {
        let end = first.checked_add(count)?;
        (end <= self.sb.blocks_count)
            .then(|| Extent::new(first * self.block_size(), count * self.block_size()))
    }

    /// Write what `plan` describes to `sink`. Source bytes that cannot be
    /// read are written as zeros and recorded as unreadable, block by
    /// block, so one bad sector costs one block. The source is never
    /// written: it is a [`BlockRead`].
    pub fn materialize(
        &self,
        source: &dyn BlockRead,
        plan: &Plan,
        sink: &mut dyn io::Write,
    ) -> Result<MaterializeReport, SalvageError> {
        let mut report = MaterializeReport::default();
        let limit = plan
            .valid_length
            .unwrap_or(u64::MAX)
            .min(plan.length.unwrap_or(u64::MAX));
        if let (Some(valid), Some(length)) = (plan.valid_length, plan.length) {
            if valid > length {
                return Err(SalvageError::InvalidPlan(format!(
                    "valid length {valid} exceeds the final length {length}"
                )));
            }
        }
        for step in &plan.steps {
            if report.written >= limit {
                break;
            }
            let room = limit - report.written;
            match step {
                PlanStep::Extent(extent) => {
                    let extent = Extent::new(extent.offset, extent.length.min(room));
                    self.copy_out(source, extent, sink, &mut report)?;
                }
                PlanStep::Clusters { first, count } => {
                    let extent = self.blocks(*first, *count).ok_or_else(|| {
                        SalvageError::InvalidPlan(format!(
                            "blocks {first}..{} lie outside the {}-block volume",
                            first.saturating_add(*count),
                            self.sb.blocks_count
                        ))
                    })?;
                    let extent = Extent::new(extent.offset, extent.length.min(room));
                    self.copy_out(source, extent, sink, &mut report)?;
                }
                PlanStep::Zeros(length) => {
                    zeros(sink, (*length).min(room))?;
                    report.push((*length).min(room), None, true);
                }
                PlanStep::Bytes(bytes) => {
                    let take = (bytes.len() as u64).min(room) as usize;
                    sink.write_all(&bytes[..take]).map_err(SalvageError::Sink)?;
                    report.push(take as u64, None, true);
                }
            }
        }
        if let Some(length) = plan.length {
            if report.written < length {
                let rest = length - report.written;
                zeros(sink, rest)?;
                report.push(rest, None, true);
            }
        }
        Ok(report)
    }

    fn copy_out(
        &self,
        source: &dyn BlockRead,
        extent: Extent,
        sink: &mut dyn io::Write,
        report: &mut MaterializeReport,
    ) -> Result<(), SalvageError> {
        if extent.end().is_none_or(|end| end > source.size_bytes()) {
            return Err(SalvageError::InvalidPlan(format!(
                "{} byte(s) at {} lie outside the {}-byte source",
                extent.length,
                extent.offset,
                source.size_bytes()
            )));
        }
        let mut done = 0;
        while done < extent.length {
            let take = (extent.length - done).min(CHUNK);
            let mut bytes = vec![0u8; take as usize];
            let at = extent.offset + done;
            if source.read_at(at, &mut bytes).is_ok() {
                sink.write_all(&bytes).map_err(SalvageError::Sink)?;
                report.push(take, Some(Extent::new(at, take)), true);
            } else {
                self.copy_by_block(source, Extent::new(at, take), sink, report)?;
            }
            done += take;
        }
        Ok(())
    }

    /// A chunk that failed to read, retried a block at a time.
    fn copy_by_block(
        &self,
        source: &dyn BlockRead,
        extent: Extent,
        sink: &mut dyn io::Write,
        report: &mut MaterializeReport,
    ) -> Result<(), SalvageError> {
        let step = self.block_size();
        let mut done = 0;
        while done < extent.length {
            let at = extent.offset + done;
            let take = (extent.length - done).min(step - at % step);
            let mut bytes = vec![0u8; take as usize];
            let readable = source.read_at(at, &mut bytes).is_ok();
            if !readable {
                bytes.fill(0);
            }
            sink.write_all(&bytes).map_err(SalvageError::Sink)?;
            report.push(take, Some(Extent::new(at, take)), readable);
            done += take;
        }
        Ok(())
    }
}

fn zeros(sink: &mut dyn io::Write, mut length: u64) -> Result<(), SalvageError> {
    let block = vec![0u8; CHUNK.min(length) as usize];
    while length > 0 {
        let take = length.min(CHUNK) as usize;
        sink.write_all(&block[..take]).map_err(SalvageError::Sink)?;
        length -= take as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::FsFlavor;
    use crate::salvage::test_volume::Memory;
    use std::sync::Arc;

    fn pattern(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(29) ^ seed)
            .collect()
    }

    struct Volume {
        memory: Arc<Memory>,
        a: u32,
        dir: u32,
        b: u32,
        sparse: u32,
    }

    /// `/a` (5000 bytes), `/dir/b` (3 blocks), `/sparse` (one block at
    /// 1 MiB), and a symlink, on 2 KiB-block ext4.
    fn volume() -> Volume {
        let memory = Memory::format(FsFlavor::Ext4, 32, 2048);
        let (a, dir, b, sparse) = {
            let fs = memory.mount();
            let a = fs.apply_create("/a", 0o644).unwrap();
            fs.apply_pwrite("/a", 0, &pattern(1, 5000)).unwrap();
            let dir = fs.apply_mkdir("/dir", 0o755).unwrap();
            let b = fs.apply_create("/dir/b", 0o644).unwrap();
            fs.apply_pwrite("/dir/b", 0, &pattern(2, 6144)).unwrap();
            let sparse = fs.apply_create("/sparse", 0o644).unwrap();
            fs.apply_pwrite("/sparse", 1 << 20, &pattern(3, 100))
                .unwrap();
            fs.apply_symlink("a", "/link").unwrap();
            (a, dir, b, sparse)
        };
        Volume {
            memory,
            a,
            dir,
            b,
            sparse,
        }
    }

    fn candidates(salvage: &Ext4Salvage, memory: &Memory) -> Vec<Candidate> {
        let mut found = Vec::new();
        let size = fs_core::BlockRead::size_bytes(memory);
        salvage
            .find_candidates(memory, Extent::new(0, size), &mut found)
            .unwrap();
        found
    }

    fn by_ino(found: &[Candidate], ino: u32) -> &Candidate {
        found.iter().find(|c| c.ino() == Some(ino)).unwrap()
    }

    fn recover(
        salvage: &Ext4Salvage,
        memory: &Memory,
        candidate: &Candidate,
    ) -> (Vec<u8>, MaterializeReport) {
        let walk = salvage.walk_extents(memory, candidate).unwrap();
        let mut out = Vec::new();
        let report = salvage.materialize(memory, &walk.plan(), &mut out).unwrap();
        (out, report)
    }

    #[test]
    fn every_inode_and_superblock_copy_is_a_candidate() {
        let v = volume();
        let salvage = Ext4Salvage::new(&*v.memory).unwrap();
        let found = candidates(&salvage, &v.memory);
        let superblocks = found
            .iter()
            .filter(|c| matches!(c.detail, CandidateDetail::Superblock { .. }))
            .count();
        assert_eq!(superblocks, 1, "a one-group volume has only the primary");
        assert_eq!(by_ino(&found, 2).kind, CandidateKind::Directory);
        assert_eq!(by_ino(&found, v.a).kind, CandidateKind::File);
        assert_eq!(by_ino(&found, v.dir).kind, CandidateKind::Directory);
        assert_eq!(by_ino(&found, v.b).state, CandidateState::Live);
        assert_eq!(by_ino(&found, v.a).integrity, Integrity::Verified);
        assert_eq!(by_ino(&found, v.a).size, Some(5000));
    }

    #[test]
    fn a_file_is_rebuilt_from_its_walk() {
        let v = volume();
        let salvage = Ext4Salvage::new(&*v.memory).unwrap();
        let found = candidates(&salvage, &v.memory);
        let (bytes, report) = recover(&salvage, &v.memory, by_ino(&found, v.a));
        assert_eq!(bytes, pattern(1, 5000));
        assert_eq!(report.unreadable(), 0);
        let (bytes, _) = recover(&salvage, &v.memory, by_ino(&found, v.b));
        assert_eq!(bytes, pattern(2, 6144));
    }

    #[test]
    fn a_sparse_file_keeps_its_hole_as_zeros() {
        let v = volume();
        let salvage = Ext4Salvage::new(&*v.memory).unwrap();
        let found = candidates(&salvage, &v.memory);
        let walk = salvage
            .walk_extents(&*v.memory, by_ino(&found, v.sparse))
            .unwrap();
        assert_eq!(walk.claimed, (1 << 20) + 100);
        assert!(
            matches!(walk.pieces[0], Piece::Zeros(n) if n == 1 << 20),
            "{walk:?}"
        );
        let (bytes, _) = recover(&salvage, &v.memory, by_ino(&found, v.sparse));
        assert!(bytes[..1 << 20].iter().all(|&b| b == 0));
        assert_eq!(&bytes[1 << 20..], pattern(3, 100));
    }

    #[test]
    fn the_tree_names_every_live_inode_and_its_parent() {
        let v = volume();
        let salvage = Ext4Salvage::new(&*v.memory).unwrap();
        let found = candidates(&salvage, &v.memory);
        let tree = salvage.rebuild_tree(&*v.memory, &found).unwrap();
        let node = |ino| tree.nodes.iter().find(|n| n.ino == ino).unwrap();
        assert_eq!(node(2).parents[0].parent, Parent::Root);
        let dir_at = by_ino(&found, v.dir).location.offset;
        let root_at = by_ino(&found, 2).location.offset;
        assert_eq!(node(v.b).parents.len(), 1);
        assert_eq!(node(v.b).parents[0].parent, Parent::Candidate(dir_at));
        assert_eq!(node(v.b).parents[0].name, b"b");
        assert_eq!(node(v.dir).parents.len(), 1, "{:?}", node(v.dir));
        assert_eq!(node(v.dir).parents[0].parent, Parent::Candidate(root_at));
        assert_eq!(node(v.dir).parents[0].name, b"dir");
        assert!(
            tree.orphans().all(|n| n.ino < 11),
            "{:?}",
            tree.orphans().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_destroyed_directory_leaves_orphans_whose_contents_survive() {
        let v = volume();
        let salvage = Ext4Salvage::new(&*v.memory).unwrap();
        let found = candidates(&salvage, &v.memory);
        let walk = salvage
            .walk_extents(&*v.memory, by_ino(&found, v.dir))
            .unwrap();
        let block = walk.extents[0];
        v.memory
            .poke(block.offset, &vec![0u8; block.length as usize]);
        let tree = salvage.rebuild_tree(&*v.memory, &found).unwrap();
        let b = tree.nodes.iter().find(|n| n.ino == v.b).unwrap();
        assert!(b.parents.is_empty(), "{b:?}");
        let (bytes, _) = recover(&salvage, &v.memory, by_ino(&found, v.b));
        assert_eq!(bytes, pattern(2, 6144));
    }

    /// The kernel deletes an inode by setting `i_dtime` with
    /// `i_links_count` at zero and its mode in place
    /// (`tests/salvage_verbs_oracle.rs`); this crate's own unlink clears the
    /// slot, so the kernel's way is written here by hand.
    #[test]
    fn an_inode_with_no_links_and_a_deletion_time_is_deleted() {
        let v = volume();
        let salvage = Ext4Salvage::new(&*v.memory).unwrap();
        let at = by_ino(&candidates(&salvage, &v.memory), v.a)
            .location
            .offset;
        v.memory.poke(at + 0x1A, &0u16.to_le_bytes()); // i_links_count
        v.memory.poke(at + 0x14, &1_700_000_000u32.to_le_bytes()); // i_dtime
        let found = candidates(&salvage, &v.memory);
        let a = by_ino(&found, v.a);
        assert_eq!(a.state, CandidateState::Deleted);
        assert_eq!(
            a.integrity,
            Integrity::Failed,
            "the pokes broke its checksum"
        );
    }

    #[test]
    fn damage_names_the_inode_whose_checksum_fails() {
        let v = volume();
        let salvage = Ext4Salvage::new(&*v.memory).unwrap();
        let found = candidates(&salvage, &v.memory);
        let a = by_ino(&found, v.a).location;
        v.memory.flip(a.offset + 0x20); // i_ctime
        let mut damage = Vec::new();
        let report = salvage.report_damage(&*v.memory, &mut damage).unwrap();
        assert_eq!(report.reported, damage.len() as u64);
        let name = format!("inode {}", v.a);
        assert!(
            damage
                .iter()
                .any(|d| d.kind == DamageKind::Checksum && d.structure == name),
            "{damage:?}"
        );
    }

    #[test]
    fn a_full_sink_stops_the_scan_and_says_where() {
        let v = volume();
        let salvage = Ext4Salvage::new(&*v.memory).unwrap();
        let mut taken = 0;
        let mut sink = |_: Candidate| {
            taken += 1;
            if taken == 3 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let size = fs_core::BlockRead::size_bytes(&*v.memory);
        let report = salvage
            .find_candidates(&*v.memory, Extent::new(0, size), &mut sink)
            .unwrap();
        assert_eq!(report.candidates, 3);
        assert!(report.stopped_at.is_some());
    }

    /// A source with one unreadable block.
    struct BadBlock<'a> {
        inner: &'a Memory,
        bad: Extent,
    }

    impl fs_core::BlockRead for BadBlock<'_> {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
            let end = offset + buf.len() as u64;
            if offset < self.bad.end().unwrap() && end > self.bad.offset {
                return Err(fs_core::Error::Custom("bad sector".to_string()));
            }
            fs_core::BlockRead::read_at(self.inner, offset, buf)
        }
        fn size_bytes(&self) -> u64 {
            fs_core::BlockRead::size_bytes(self.inner)
        }
    }

    #[test]
    fn an_unreadable_block_costs_that_block_and_no_more() {
        let v = volume();
        let salvage = Ext4Salvage::new(&*v.memory).unwrap();
        let found = candidates(&salvage, &v.memory);
        let walk = salvage
            .walk_extents(&*v.memory, by_ino(&found, v.b))
            .unwrap();
        let second = Extent::new(walk.extents[0].offset + 2048, 2048);
        let source = BadBlock {
            inner: &v.memory,
            bad: second,
        };
        let mut out = Vec::new();
        let report = salvage
            .materialize(&source, &walk.plan(), &mut out)
            .unwrap();
        assert_eq!(report.unreadable(), 2048);
        let want = pattern(2, 6144);
        assert_eq!(out[..2048], want[..2048]);
        assert!(out[2048..4096].iter().all(|&b| b == 0));
        assert_eq!(out[4096..], want[4096..]);
    }

    #[test]
    fn a_plan_past_the_volume_is_refused() {
        let v = volume();
        let salvage = Ext4Salvage::new(&*v.memory).unwrap();
        let plan = Plan {
            steps: vec![PlanStep::Clusters {
                first: salvage.superblock().blocks_count,
                count: 1,
            }],
            length: None,
            valid_length: None,
        };
        assert!(matches!(
            salvage.materialize(&*v.memory, &plan, &mut Vec::new()),
            Err(SalvageError::InvalidPlan(_))
        ));
    }
}
