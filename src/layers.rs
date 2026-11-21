//! The three API layers of rust-fs-core (`fs_core::layers`) for ext2, ext3
//! and ext4 volumes.
//!
//! [`Ext4Volume`] is the volume layer ([`VolumeAPI`], [`OpenVolume`]), the
//! primitive layer ([`PrimitiveAPI`], with [`Ext4Adapter`] as its
//! [`SalvageAdapter`]) and the file layer ([`FileAPI`]). It is built on
//! this crate's own pieces: the damage-tolerant reads of [`crate::salvage`]
//! below the namespace, and [`Filesystem`] for the namespace of a healthy
//! volume. `docs/three-layers.md` maps the rest of the crate's API onto
//! the layers.
//!
//! Nothing here writes. The source is a [`BlockRead`], and the mount the
//! file layer uses is given a device whose writes fail.

use crate::features::FsFlavor;
use crate::fs::Filesystem;
use crate::inode::{InodeFlags, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use crate::salvage::{self as native, Ext4Salvage};
use crate::superblock::{Superblock, SUPERBLOCK_OFFSET, SUPERBLOCK_SIZE};
use fs_core::layers::{
    check_bounds, ApiError, Candidate, CandidateKind, CandidateState, Capabilities, ClusterLayout,
    Damage, DamageKind, DamageReport, DirEntry, Extent, ExtentWalk, FileAPI, Geometry, Integrity,
    Metadata, NodeKind, OpenVolume, Parent, ParentClaim, PrimitiveAPI, Probe, Recognition,
    SalvageAdapter, SalvageError, ScanReport, Severity, Sink, Structure, StructureRole,
    SuperblockCopy, Timestamp, Tree, TreeNode, VolumeAPI, VolumeHealth, VolumeSupport, WalkEnd,
    WalkExtent,
};
use fs_core::BlockRead;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

/// The root directory's inode number.
const ROOT_INO: u32 = 2;

/// Why a layer call failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum LayerError {
    /// A refusal the layers share: out of bounds, not found, not a
    /// directory, no namespace on a volume opened for salvage.
    Api(ApiError),
    /// The source failed a read.
    Device(fs_core::Error),
    /// This crate refused: a mount that did not validate, a feature it
    /// cannot read.
    Ext4(crate::Error),
    /// The damage-tolerant reads found no geometry.
    Salvage(native::SalvageError),
}

impl fmt::Display for LayerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api(error) => error.fmt(formatter),
            Self::Device(error) => error.fmt(formatter),
            Self::Ext4(error) => error.fmt(formatter),
            Self::Salvage(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for LayerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Api(error) => Some(error),
            Self::Device(error) => Some(error),
            Self::Ext4(error) => Some(error),
            Self::Salvage(error) => Some(error),
        }
    }
}

impl From<ApiError> for LayerError {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl From<fs_core::Error> for LayerError {
    fn from(error: fs_core::Error) -> Self {
        Self::Device(error)
    }
}

impl From<crate::Error> for LayerError {
    fn from(error: crate::Error) -> Self {
        Self::Ext4(error)
    }
}

impl From<native::SalvageError> for LayerError {
    fn from(error: native::SalvageError) -> Self {
        Self::Salvage(error)
    }
}

/// A [`BlockRead`] lent to [`Filesystem`] as its device. Writes fail: the
/// device keeps the default `write_at`, and `is_writable` stays false.
struct ReadOnlySource<S>(Arc<S>);

impl<S: BlockRead> crate::block_io::BlockDevice for ReadOnlySource<S> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> crate::Result<()> {
        self.0
            .read_at(offset, buf)
            .map_err(|error| crate::Error::Io(std::io::Error::other(error.to_string())))
    }

    fn size_bytes(&self) -> u64 {
        self.0.size_bytes()
    }
}

/// An ext2, ext3 or ext4 volume, through the three layers.
pub struct Ext4Volume<S> {
    source: Arc<S>,
    /// The superblock copy the geometry came from.
    chosen: native::SuperblockCopy,
    sb: Superblock,
    /// The mount the file layer reads through; `None` when the volume was
    /// opened for salvage and the mount refused it.
    fs: Option<Filesystem>,
    /// Why the mount refused, when it did.
    refusal: Option<String>,
}

impl<S: BlockRead + 'static> Ext4Volume<S> {
    fn build(source: S, mount_required: bool) -> Result<Self, LayerError> {
        let source = Arc::new(source);
        let chosen = native::find_superblock(&*source)?;
        let sb = chosen
            .superblock
            .clone()
            .ok_or(LayerError::Salvage(native::SalvageError::NoGeometry))?;
        let mounted = if chosen.group == 0 && chosen.is_sound() {
            Filesystem::mount(Arc::new(ReadOnlySource(source.clone())))
        } else {
            Err(crate::Error::Corrupt(
                "the primary superblock is damaged; the geometry came from a backup",
            ))
        };
        let (fs, refusal) = match mounted {
            Ok(fs) => (Some(fs), None),
            Err(error) if mount_required => return Err(error.into()),
            Err(error) => (None, Some(error.to_string())),
        };
        Ok(Self {
            source,
            chosen,
            sb,
            fs,
            refusal,
        })
    }

    /// The superblock copy the geometry came from: the primary, or the
    /// backup a volume opened for salvage fell back to.
    pub fn chosen_superblock(&self) -> &native::SuperblockCopy {
        &self.chosen
    }

    /// The source, shared.
    pub fn source_arc(&self) -> &Arc<S> {
        &self.source
    }

    /// The crate's own mount, for what the layers do not cover; `None` on a
    /// volume opened for salvage whose mount was refused.
    pub fn filesystem(&self) -> Option<&Filesystem> {
        self.fs.as_ref()
    }

    fn fs(&self) -> Result<&Filesystem, LayerError> {
        self.fs
            .as_ref()
            .ok_or(LayerError::Api(ApiError::Unsupported(
                "the namespace of a volume whose mount was refused",
            )))
    }

    fn block_size(&self) -> u64 {
        u64::from(self.sb.block_size())
    }
}

impl<S: BlockRead + 'static> VolumeAPI for Ext4Volume<S> {
    type Error = LayerError;
    type Superblock = Superblock;

    fn format(&self) -> &'static str {
        FsFlavor::detect(self.sb.feature_compat, self.sb.feature_incompat).name()
    }

    fn size_bytes(&self) -> u64 {
        let declared = self.sb.filesystem_bytes().unwrap_or(u64::MAX);
        self.source.size_bytes().min(declared)
    }

    fn read_volume_bytes(&self, offset: u64, output: &mut [u8]) -> Result<(), LayerError> {
        check_bounds(offset, output.len(), self.size_bytes())?;
        self.source.read_at(offset, output)?;
        Ok(())
    }

    fn geometry(&self) -> Result<Geometry, LayerError> {
        let label = self.sb.volume_name.trim_end_matches('\0').to_string();
        Ok(Geometry {
            sector_size: 512,
            allocation_unit: self.block_size(),
            allocation_units: self.sb.blocks_count,
            declared_bytes: self.sb.filesystem_bytes().unwrap_or(u64::MAX),
            layout: Some(block_layout(&self.sb)),
            label: (!label.is_empty()).then_some(label),
            serial: Some(uuid_text(&self.sb.uuid)),
        })
    }

    fn superblock(&self) -> Result<Superblock, LayerError> {
        Ok(self.sb.clone())
    }

    fn superblocks(&self) -> Result<Vec<SuperblockCopy<Superblock>>, LayerError> {
        Ok(native::superblock_copies(&*self.source, &self.sb)
            .into_iter()
            .enumerate()
            .map(|(copy, found)| SuperblockCopy {
                copy: copy as u32,
                location: extent(found.location),
                integrity: integrity(found.integrity),
                superblock: found.superblock,
            })
            .collect())
    }

    fn support(&self) -> Result<VolumeSupport, LayerError> {
        let mut unsupported = Vec::new();
        if let Err(error) =
            crate::features::check_mountable(self.sb.feature_incompat, self.sb.feature_ro_compat)
        {
            unsupported.push(error.to_string());
        }
        if let Some(refusal) = &self.refusal {
            unsupported.push(format!("the namespace: {refusal}"));
        }
        Ok(VolumeSupport {
            files: self.fs.is_some(),
            salvage: Capabilities {
                missing: Vec::new(),
            },
            unsupported,
        })
    }

    fn health(&self) -> Result<VolumeHealth, LayerError> {
        // Quick: the superblocks, descriptor tables and root directory.
        // Every inode is the damage report's (PrimitiveAPI::salvage).
        let findings = Ext4Salvage::from_superblock(self.sb.clone())
            .quick_damage(&*self.source)
            .into_iter()
            .map(damage)
            .collect();
        Ok(VolumeHealth::from_findings(findings))
    }
}

impl<S: BlockRead + 'static> OpenVolume for Ext4Volume<S> {
    type Source = S;

    fn probe(source: &dyn BlockRead) -> Result<Probe, fs_core::Error> {
        let mut raw = vec![0u8; SUPERBLOCK_SIZE];
        source.read_at(SUPERBLOCK_OFFSET, &mut raw)?;
        let primary = native::read_superblock_at(source, 0, SUPERBLOCK_OFFSET);
        let (recognition, declared, detail) = if primary.is_sound() {
            let sb = primary.superblock.as_ref().unwrap();
            (
                Recognition::Yes,
                sb.filesystem_bytes(),
                format!("{} superblock at byte 1024", flavor_name(sb)),
            )
        } else if let Ok(found) = native::find_superblock(source) {
            let sb = found.superblock.as_ref().unwrap();
            (
                Recognition::Damaged,
                sb.filesystem_bytes(),
                format!(
                    "the primary superblock is damaged; a {} copy is in group {}",
                    flavor_name(sb),
                    found.group
                ),
            )
        } else {
            (Recognition::No, None, "no ext2/3/4 superblock".to_string())
        };
        Ok(Probe {
            format: "ext4",
            recognition,
            declared_bytes: declared,
            detail,
        })
    }

    fn open(source: S) -> Result<Self, LayerError> {
        Self::build(source, true)
    }

    fn open_for_salvage(source: S) -> Result<Self, LayerError> {
        Self::build(source, false)
    }
}

/// A guess at where a usable superblock is, for a volume whose own copies
/// are gone or disagree: a byte offset holding 1024 bytes that parse as an
/// ext2/3/4 superblock. The superblock says everything else -- block size,
/// group layout, inode size -- so the offset is the whole hypothesis.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ext4Hypothesis {
    /// The byte offset of the superblock copy.
    pub offset: u64,
    /// The group the copy says it was written into (`s_block_group_nr`),
    /// or 0 for the primary.
    pub group: u64,
    /// Its checksum held, or it keeps none.
    pub sound: bool,
}

impl<S: BlockRead + 'static> PrimitiveAPI for Ext4Volume<S> {
    type Adapter = Ext4Adapter;
    type Hypothesis = Ext4Hypothesis;

    /// Every superblock copy that parses at a place the format puts one:
    /// the layout's copies, then the default backup groups at every block
    /// size, sound ones first.
    fn hypotheses(&self) -> Result<Vec<Ext4Hypothesis>, SalvageError> {
        let source = &*self.source;
        let mut found: Vec<Ext4Hypothesis> = Vec::new();
        let mut push = |copy: native::SuperblockCopy| {
            if copy.superblock.is_some() && !found.iter().any(|h| h.offset == copy.location.offset)
            {
                found.push(Ext4Hypothesis {
                    offset: copy.location.offset,
                    group: copy.group,
                    sound: copy.is_sound(),
                });
            }
        };
        for copy in native::superblock_copies(source, &self.sb) {
            push(copy);
        }
        for copy in native::probe_backup_copies(source) {
            push(copy);
        }
        found.sort_by_key(|h| !h.sound);
        Ok(found)
    }

    fn adapter_with(&self, hypothesis: &Ext4Hypothesis) -> Result<Ext4Adapter, SalvageError> {
        let copy = native::read_superblock_at(&*self.source, hypothesis.group, hypothesis.offset);
        copy.superblock
            .map(Ext4Adapter::new)
            .ok_or(SalvageError::NoGeometry)
    }

    fn source(&self) -> &dyn BlockRead {
        &*self.source
    }

    fn adapter(&self) -> Result<Ext4Adapter, SalvageError> {
        Ok(Ext4Adapter {
            salvage: Ext4Salvage::from_superblock(self.sb.clone()),
        })
    }

    fn structures(&self) -> Result<Vec<Structure>, SalvageError> {
        let source = &*self.source;
        let block_size = self.block_size();
        let mut found = Vec::new();
        for (copy, sb) in native::superblock_copies(source, &self.sb)
            .into_iter()
            .enumerate()
        {
            found.push(Structure {
                name: if sb.group == 0 {
                    "primary superblock".to_string()
                } else {
                    format!("superblock backup in group {}", sb.group)
                },
                role: StructureRole::Superblock,
                location: extent(sb.location),
                copy: copy as u32,
                integrity: integrity(sb.integrity),
            });
        }
        let tables = native::descriptor_table_copies(&self.sb);
        let inode_table_blocks = (u64::from(self.sb.inodes_per_group)
            * u64::from(self.sb.inode_size))
        .div_ceil(block_size);
        for copy in 0..tables {
            for record in native::group_descriptors(source, &self.sb, copy) {
                found.push(Structure {
                    name: format!("group descriptor {}", record.group),
                    role: StructureRole::RecordTable,
                    location: extent(record.location),
                    copy: copy as u32,
                    integrity: integrity(record.integrity),
                });
                let Some(descriptor) = record.descriptor.filter(|_| copy == 0) else {
                    continue;
                };
                let group = record.group;
                for (name, role, block, length) in [
                    (
                        "block bitmap",
                        StructureRole::AllocationMap,
                        descriptor.block_bitmap,
                        1,
                    ),
                    (
                        "inode bitmap",
                        StructureRole::AllocationMap,
                        descriptor.inode_bitmap,
                        1,
                    ),
                    (
                        "inode table",
                        StructureRole::RecordTable,
                        descriptor.inode_table,
                        inode_table_blocks,
                    ),
                ] {
                    found.push(Structure {
                        name: format!("{name} of group {group}"),
                        role,
                        location: Extent::new(
                            block.saturating_mul(block_size),
                            length * block_size,
                        ),
                        copy: 0,
                        integrity: Integrity::Unchecked,
                    });
                }
            }
        }
        if let Ok(journal) = native::scan_journal(source, &self.sb) {
            if let Some(location) = journal.superblock_location {
                found.push(Structure {
                    name: "journal superblock".to_string(),
                    role: StructureRole::Journal,
                    location: extent(location),
                    copy: 0,
                    integrity: if journal.superblock.is_some() {
                        Integrity::Unchecked
                    } else {
                        Integrity::Failed
                    },
                });
            }
        }
        Ok(found)
    }
}

impl<S: BlockRead + 'static> FileAPI for Ext4Volume<S> {
    type Node = u32;

    fn root(&self) -> Result<u32, LayerError> {
        self.fs()?;
        Ok(ROOT_INO)
    }

    fn read_dir(&self, directory: &u32) -> Result<Vec<DirEntry<u32>>, LayerError> {
        let fs = self.fs()?;
        let inode = fs.stat_ino(*directory)?;
        if !inode.is_dir() {
            return Err(ApiError::NotADirectory(format!("inode {directory}")).into());
        }
        let mut entries = Vec::new();
        for entry in fs.read_dir_ino(*directory)? {
            if entry.name == b"." || entry.name == b".." {
                continue;
            }
            let child = fs.stat_ino(entry.inode)?;
            let (name, lossy) = match String::from_utf8(entry.name.clone()) {
                Ok(name) => (name, false),
                Err(_) => (String::from_utf8_lossy(&entry.name).into_owned(), true),
            };
            entries.push(DirEntry {
                name,
                bytes: entry.name.clone(),
                lossy,
                kind: node_kind(child.mode),
                size: if child.is_dir() { 0 } else { child.size },
                node: entry.inode,
            });
        }
        Ok(entries)
    }

    fn metadata(&self, node: &u32) -> Result<Metadata, LayerError> {
        let inode = self.fs()?.stat_ino(*node)?;
        let stamp = |seconds: i64, nanoseconds: u32| Timestamp {
            seconds,
            nanoseconds,
        };
        let allocated = if inode.flags & InodeFlags::HUGE_FILE.bits() != 0 {
            inode.blocks.saturating_mul(self.block_size())
        } else {
            inode.blocks.saturating_mul(512)
        };
        let mut metadata = Metadata::new(
            node_kind(inode.mode),
            if inode.is_dir() { 0 } else { inode.size },
        );
        metadata.allocated = Some(allocated);
        metadata.created = (inode.crtime != 0).then(|| stamp(inode.crtime, inode.crtime_nsec));
        metadata.modified = Some(stamp(inode.mtime, inode.mtime_nsec));
        metadata.accessed = Some(stamp(inode.atime, inode.atime_nsec));
        metadata.mode = Some(u32::from(inode.mode & 0o7777));
        metadata.links = Some(u64::from(inode.links_count));
        metadata.attributes = Some(u64::from(inode.flags));
        Ok(metadata)
    }

    fn read_at(&self, file: &u32, offset: u64, output: &mut [u8]) -> Result<usize, LayerError> {
        let fs = self.fs()?;
        let inode = fs.stat_ino(*file)?;
        if inode.is_dir() {
            return Err(ApiError::IsADirectory(format!("inode {file}")).into());
        }
        if offset >= inode.size {
            return Ok(0);
        }
        let want = (inode.size - offset).min(output.len() as u64) as usize;
        let mut done = 0;
        while done < want {
            let read = fs.read_ino(*file, offset + done as u64, &mut output[done..want])?;
            if read == 0 {
                return Err(LayerError::Ext4(crate::Error::Corrupt(
                    "the file ended before its size",
                )));
            }
            done += read;
        }
        Ok(want)
    }

    fn read_link(&self, link: &u32) -> Result<Vec<u8>, LayerError> {
        Ok(self.fs()?.read_link_ino(*link)?)
    }
}

/// The ext4 part of a candidate: what the inode or superblock copy says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Ext4Detail {
    #[serde(rename_all = "camelCase")]
    Inode {
        ino: u32,
        mode: u16,
        links: u16,
        /// `i_dtime`: when the inode was deleted, or 0.
        dtime: i64,
        mtime: i64,
        flags: u32,
        generation: u32,
    },
    #[serde(rename_all = "camelCase")]
    Superblock { group: u64 },
}

/// [`Ext4Salvage`] through the shared [`SalvageAdapter`] trait.
#[derive(Clone, Debug)]
pub struct Ext4Adapter {
    salvage: Ext4Salvage,
}

impl Ext4Adapter {
    /// The adapter for a superblock the caller chose.
    pub fn new(sb: Superblock) -> Self {
        Self {
            salvage: Ext4Salvage::from_superblock(sb),
        }
    }

    /// The crate's own verbs, which keep what the shared types cannot say
    /// yet: a walk's holes, and the names parent claims give.
    pub fn native(&self) -> &Ext4Salvage {
        &self.salvage
    }

    fn back(candidate: &Candidate<Ext4Detail>) -> native::Candidate {
        native::Candidate {
            location: native::Extent::new(candidate.location.offset, candidate.location.length),
            kind: match candidate.kind {
                CandidateKind::File => native::CandidateKind::File,
                CandidateKind::Directory => native::CandidateKind::Directory,
                CandidateKind::Structure => native::CandidateKind::Structure,
                _ => native::CandidateKind::Other,
            },
            state: match candidate.state {
                CandidateState::Live => native::CandidateState::Live,
                CandidateState::Deleted => native::CandidateState::Deleted,
                _ => native::CandidateState::Unknown,
            },
            integrity: match candidate.integrity {
                Integrity::Verified => native::Integrity::Verified,
                Integrity::Failed => native::Integrity::Failed,
                _ => native::Integrity::Unchecked,
            },
            name: candidate.name.clone(),
            size: candidate.size,
            detail: match &candidate.detail {
                Ext4Detail::Inode {
                    ino,
                    mode,
                    links,
                    dtime,
                    mtime,
                    flags,
                    generation,
                } => native::CandidateDetail::Inode(native::InodeDetail {
                    ino: *ino,
                    mode: *mode,
                    links: *links,
                    dtime: *dtime,
                    mtime: *mtime,
                    flags: *flags,
                    generation: *generation,
                }),
                Ext4Detail::Superblock { group } => {
                    native::CandidateDetail::Superblock { group: *group }
                }
            },
        }
    }
}

fn salvage_error(error: native::SalvageError) -> SalvageError {
    match error {
        native::SalvageError::NoGeometry => SalvageError::NoGeometry,
        native::SalvageError::OutOfRange(_) => SalvageError::Bounds,
        native::SalvageError::Sink(error) => SalvageError::Sink(error),
        other => SalvageError::Device(fs_core::Error::Custom(other.to_string())),
    }
}

impl SalvageAdapter for Ext4Adapter {
    type Detail = Ext4Detail;

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            missing: Vec::new(),
        }
    }

    fn find_candidates(
        &self,
        source: &dyn BlockRead,
        range: Extent,
        sink: &mut dyn Sink<Candidate<Ext4Detail>>,
    ) -> Result<ScanReport, SalvageError> {
        if range.end().is_none_or(|end| end > source.size_bytes()) {
            return Err(SalvageError::Bounds);
        }
        let mut forward = |found: native::Candidate| sink.accept(candidate(found));
        let report = self
            .salvage
            .find_candidates(
                source,
                native::Extent::new(range.offset, range.length),
                &mut forward,
            )
            .map_err(salvage_error)?;
        Ok(ScanReport {
            candidates: report.candidates,
            unreadable: report.unreadable.into_iter().map(extent).collect(),
            stopped_at: report.stopped_at,
        })
    }

    fn walk_extents(
        &self,
        source: &dyn BlockRead,
        candidate: &Candidate<Ext4Detail>,
    ) -> Result<ExtentWalk, SalvageError> {
        if let Ext4Detail::Superblock { .. } = candidate.detail {
            // A superblock copy claims no storage beyond its own bytes.
            return Ok(ExtentWalk {
                extents: vec![WalkExtent::stored(0, candidate.location)],
                claimed: candidate.location.length,
                end: WalkEnd::Complete,
            });
        }
        let walk = self
            .salvage
            .walk_extents(source, &Self::back(candidate))
            .map_err(salvage_error)?;
        // Every stored byte at its own file offset: holes are the gaps
        // between extents, uninitialized extents are flagged, and the
        // claim is `i_size`.
        let mut extents: Vec<WalkExtent> = Vec::new();
        let mut offset = 0u64;
        for piece in &walk.pieces {
            match piece {
                native::Piece::Stored(stored) => {
                    let joins = extents.last().is_some_and(|last| {
                        !last.uninitialized
                            && last.end() == Some(offset)
                            && last.source.end() == Some(stored.offset)
                    });
                    if joins {
                        extents.last_mut().unwrap().source.length += stored.length;
                    } else {
                        extents.push(WalkExtent::stored(offset, extent(*stored)));
                    }
                }
                native::Piece::Uninitialized(stored) => {
                    let mut walked = WalkExtent::stored(offset, extent(*stored));
                    walked.uninitialized = true;
                    extents.push(walked);
                }
                native::Piece::Inline(_) | native::Piece::Zeros(_) => {}
            }
            offset += piece.length();
        }
        let claimed = walk.claimed;
        Ok(ExtentWalk {
            extents,
            claimed,
            end: match walk.end {
                native::MapEnd::Complete => WalkEnd::Complete,
                native::MapEnd::Broken => WalkEnd::Broken,
                native::MapEnd::OutOfRange => WalkEnd::OutOfRange,
                native::MapEnd::Cycle => WalkEnd::Cycle,
                native::MapEnd::Unreadable(at) => WalkEnd::Unreadable(extent(at)),
            },
        })
    }

    fn rebuild_tree(
        &self,
        source: &dyn BlockRead,
        candidates: &[Candidate<Ext4Detail>],
    ) -> Result<Tree, SalvageError> {
        let native_candidates: Vec<native::Candidate> = candidates.iter().map(Self::back).collect();
        let tree = self
            .salvage
            .rebuild_tree(source, &native_candidates)
            .map_err(salvage_error)?;
        Ok(Tree {
            nodes: candidates
                .iter()
                .map(|candidate| TreeNode {
                    candidate: candidate.location.offset,
                    parents: tree
                        .node(candidate.location.offset)
                        .map(|node| {
                            node.parents
                                .iter()
                                .map(|claim| ParentClaim {
                                    parent: match claim.parent {
                                        native::Parent::Root => Parent::Root,
                                        native::Parent::Candidate(at) => Parent::Candidate(at),
                                    },
                                    name: (!claim.name.is_empty()).then(|| claim.name.clone()),
                                    state: if claim.live {
                                        CandidateState::Live
                                    } else {
                                        CandidateState::Deleted
                                    },
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect(),
        })
    }

    fn report_damage(
        &self,
        source: &dyn BlockRead,
        sink: &mut dyn Sink<Damage>,
    ) -> Result<DamageReport, SalvageError> {
        let mut forward = |found: native::Damage| sink.accept(damage(found));
        let report = self
            .salvage
            .report_damage(source, &mut forward)
            .map_err(salvage_error)?;
        Ok(DamageReport {
            reported: report.reported,
            stopped: report.stopped,
        })
    }

    fn read_extent(
        &self,
        source: &dyn BlockRead,
        extent: Extent,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, SalvageError> {
        if offset > extent.length || extent.end().is_none_or(|end| end > source.size_bytes()) {
            return Err(SalvageError::Bounds);
        }
        let count = (extent.length - offset).min(output.len() as u64) as usize;
        source
            .read_at(extent.offset + offset, &mut output[..count])
            .map_err(SalvageError::Device)?;
        Ok(count)
    }

    fn cluster_layout(&self) -> Result<ClusterLayout, SalvageError> {
        Ok(block_layout(self.salvage.superblock()))
    }
}

/// ext4's allocation units are its blocks, numbered from the volume's start.
fn block_layout(sb: &Superblock) -> ClusterLayout {
    ClusterLayout {
        first: 0,
        count: sb.blocks_count,
        size: u64::from(sb.block_size()),
        offset: 0,
    }
}

fn flavor_name(sb: &Superblock) -> &'static str {
    FsFlavor::detect(sb.feature_compat, sb.feature_incompat).name()
}

fn uuid_text(uuid: &[u8; 16]) -> String {
    let hex: Vec<String> = uuid.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..16].concat()
    )
}

fn node_kind(mode: u16) -> NodeKind {
    match mode & S_IFMT {
        S_IFREG => NodeKind::File,
        S_IFDIR => NodeKind::Directory,
        S_IFLNK => NodeKind::Symlink,
        _ => NodeKind::Special,
    }
}

fn extent(found: native::Extent) -> Extent {
    Extent::new(found.offset, found.length)
}

fn integrity(found: native::Integrity) -> Integrity {
    match found {
        native::Integrity::Verified => Integrity::Verified,
        native::Integrity::Failed => Integrity::Failed,
        native::Integrity::Unchecked => Integrity::Unchecked,
    }
}

fn damage(found: native::Damage) -> Damage {
    Damage {
        kind: match found.kind {
            native::DamageKind::Signature => DamageKind::Signature,
            native::DamageKind::Checksum => DamageKind::Checksum,
            native::DamageKind::Mismatch => DamageKind::Mismatch,
            native::DamageKind::Missing => DamageKind::Missing,
            native::DamageKind::Malformed => DamageKind::Malformed,
            native::DamageKind::Broken => DamageKind::Broken,
            native::DamageKind::Recorded => DamageKind::Recorded,
            native::DamageKind::Unreadable => DamageKind::Unreadable,
        },
        severity: match found.severity {
            native::Severity::Notice => Severity::Notice,
            native::Severity::Warning => Severity::Warning,
            native::Severity::Error => Severity::Error,
        },
        location: extent(found.location),
        structure: found.structure,
        detail: found.detail,
    }
}

fn candidate(found: native::Candidate) -> Candidate<Ext4Detail> {
    Candidate {
        location: extent(found.location),
        kind: match found.kind {
            native::CandidateKind::File => CandidateKind::File,
            native::CandidateKind::Directory => CandidateKind::Directory,
            native::CandidateKind::Structure => CandidateKind::Structure,
            native::CandidateKind::Other => CandidateKind::Other,
        },
        state: match found.state {
            native::CandidateState::Live => CandidateState::Live,
            native::CandidateState::Deleted => CandidateState::Deleted,
            native::CandidateState::Unknown => CandidateState::Unknown,
        },
        integrity: integrity(found.integrity),
        name: found.name,
        size: found.size,
        detail: match found.detail {
            native::CandidateDetail::Inode(inode) => Ext4Detail::Inode {
                ino: inode.ino,
                mode: inode.mode,
                links: inode.links,
                dtime: inode.dtime,
                mtime: inode.mtime,
                flags: inode.flags,
                generation: inode.generation,
            },
            native::CandidateDetail::Superblock { group } => Ext4Detail::Superblock { group },
        },
    }
}
