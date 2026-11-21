//! Read-only, damage-tolerant access to the on-disk structures.
//!
//! Everything else in this crate reads a volume the way a mount does: a
//! checksum that fails, a magic number that is wrong or a pointer that
//! leaves the volume refuses the read, because the structure cannot be
//! trusted. Recovery needs the opposite. This module reads each structure
//! at the place the format puts it, reports what it found **and** how far
//! it can be believed, and never stops at the first fault:
//!
//! - a structure is returned with an [`Integrity`] verdict rather than
//!   refused, so a checksum mismatch is evidence to weigh, not a wall;
//! - every primary and backup copy is listed, whether or not it parses;
//! - what was observed to be wrong is a [`Damage`] value, with where it is
//!   and what was seen there.
//!
//! The source is a [`fs_core::BlockRead`], which has no write method, so
//! nothing here can change the volume it reads: read-only is a property of
//! the type, not a flag.
//!
//! The shapes of [`Extent`], [`Integrity`], [`Damage`], [`DamageKind`] and
//! [`Severity`] follow the salvage convention the drivers in this family
//! share, so the same findings can be handed to a format-neutral caller
//! unchanged. `docs/three-layers.md` maps this crate's API onto the three
//! layers (volume, primitives, files).

pub mod dirs;
pub mod maps;
pub mod structures;
pub mod verbs;

use std::fmt;

pub use dirs::{
    read_directory, scan_dir_block, DirBlockScan, DirContext, DirectoryScan, FoundEntry,
};
pub use maps::{map_inode, BlockMap, MapEnd, MappedRun};
pub use structures::{
    descriptor_table_copies, find_superblock, group_descriptors, read_inode, read_inode_at,
    read_superblock_at, structure_damage, superblock_copies, DescriptorRecord, InodeRecord,
    SuperblockCopy,
};
pub use verbs::{
    Candidate, CandidateDetail, CandidateKind, CandidateState, DamageReport, Ext4Salvage,
    ExtentWalk, Feature, InodeDetail, MaterializeReport, MaterializedRange, Parent, ParentClaim,
    Piece, Plan, PlanStep, ScanReport, Sink, Tree, TreeNode, WalkEnd,
};

/// A byte range of the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Extent {
    pub offset: u64,
    pub length: u64,
}

impl Extent {
    pub const fn new(offset: u64, length: u64) -> Self {
        Self { offset, length }
    }

    /// One past the last byte, or `None` when that overflows a `u64`.
    pub fn end(&self) -> Option<u64> {
        self.offset.checked_add(self.length)
    }
}

/// How far a structure's bytes can be believed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Integrity {
    /// The format's checksum over the structure matched.
    Verified,
    /// The checksum did not match, or the bytes do not parse as the
    /// structure: damaged evidence, not noise.
    Failed,
    /// The volume keeps no checksum for this structure.
    Unchecked,
}

/// What kind of fault a [`Damage`] records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DamageKind {
    /// A magic number is wrong.
    Signature,
    /// A stored checksum disagrees with the bytes it covers.
    Checksum,
    /// Copies or structures that must agree do not.
    Mismatch,
    /// A required structure is absent.
    Missing,
    /// A field holds a value the format does not allow.
    Malformed,
    /// A map stops before the structure it describes is covered.
    Broken,
    /// The format itself records a fault, such as the error state.
    Recorded,
    /// The bytes could not be read.
    Unreadable,
}

/// How much a [`Damage`] matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Severity {
    /// State that may explain other damage, such as an unclean unmount.
    Notice,
    /// Damage that a redundant copy or the format's tolerance covers.
    Warning,
    /// Damage that leaves a structure unusable as stored.
    Error,
}

/// One observed fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Damage {
    pub kind: DamageKind,
    pub severity: Severity,
    /// The damaged bytes, or for a missing structure where it should be.
    pub location: Extent,
    /// The structure holding the damage, in ext4's own terms:
    /// `"superblock backup in group 3"`, `"group descriptor 12"`.
    pub structure: String,
    /// What was observed, such as stored and computed values.
    pub detail: String,
}

/// Why a salvage read could not go on.
///
/// Damage is never an error here: it comes back as [`Integrity`] and
/// [`Damage`]. These are the refusals and failures that leave nothing to
/// report as evidence.
#[derive(Debug)]
#[non_exhaustive]
pub enum SalvageError {
    /// No copy of the superblock survived, so nothing says where any other
    /// structure is.
    NoGeometry,
    /// A request names something outside the volume or the format's range,
    /// such as inode 0 or an inode past `s_inodes_count`.
    OutOfRange(String),
    /// Source bytes a request named directly could not be read. Bytes a
    /// scan or a plan meets along the way are reported as evidence instead.
    Read(String),
    /// A plan that cannot be carried out as written.
    InvalidPlan(String),
    /// The caller's output refused the bytes.
    Sink(std::io::Error),
}

impl fmt::Display for SalvageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoGeometry => formatter.write_str("no readable superblock copy was found"),
            Self::OutOfRange(what) => write!(formatter, "out of range: {what}"),
            Self::Read(what) => write!(formatter, "source read failed: {what}"),
            Self::InvalidPlan(what) => write!(formatter, "invalid plan: {what}"),
            Self::Sink(error) => write!(formatter, "output sink failed: {error}"),
        }
    }
}

impl std::error::Error for SalvageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sink(error) => Some(error),
            _ => None,
        }
    }
}

/// A volume in memory for the salvage unit tests: writable for the crate's
/// `mkfs` and its write path, readable as a salvage source.
#[cfg(test)]
pub(crate) mod test_volume {
    use crate::block_io::BlockDevice;
    use crate::features::FsFlavor;
    use crate::fs::Filesystem;
    use crate::mkfs::format_filesystem_with_flavor;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    /// The crate's `mkfs` writes only metadata, so a volume is kept as the
    /// chunks written to it: four groups of 2 KiB blocks are 128 MiB, of
    /// which a few hundred KiB are ever stored.
    const CHUNK: u64 = 4096;

    pub(crate) struct Memory {
        size: u64,
        chunks: Mutex<BTreeMap<u64, Vec<u8>>>,
    }

    impl Memory {
        pub(crate) fn format(flavor: FsFlavor, mib: u64, block_size: u32) -> Arc<Self> {
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
            Arc::new(memory)
        }

        /// The crate's own mount, to put files and directories on the
        /// volume through the write path. Drop it before reading.
        pub(crate) fn mount(self: &Arc<Self>) -> Filesystem {
            Filesystem::mount(self.clone()).unwrap()
        }

        pub(crate) fn copy(&self, offset: u64, buf: &mut [u8]) {
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

        pub(crate) fn poke(&self, offset: u64, bytes: &[u8]) {
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

        pub(crate) fn flip(&self, offset: u64) {
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
}
