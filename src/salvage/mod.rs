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

pub mod structures;

use std::fmt;

pub use structures::{
    descriptor_table_copies, find_superblock, group_descriptors, read_inode, read_superblock_at,
    structure_damage, superblock_copies, DescriptorRecord, InodeRecord, SuperblockCopy,
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
}

impl fmt::Display for SalvageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoGeometry => formatter.write_str("no readable superblock copy was found"),
            Self::OutOfRange(what) => write!(formatter, "out of range: {what}"),
        }
    }
}

impl std::error::Error for SalvageError {}
