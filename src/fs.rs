//! Top-level filesystem handle. Composes block_io + superblock + bgd + inode + extent + dir.

use crate::bgd::{self, BlockGroupDescriptor};
use crate::block_io::BlockDevice;
use crate::checksum::Checksummer;
use crate::error::{Error, Result};
use crate::features;
use crate::inode::{set_inode_time, Inode, InodeTime};
use crate::superblock::Superblock;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

/// In-memory accumulator for journaled multi-block writes. Each helper
/// mutation reads the latest version of a block (from this buffer if
/// already touched, else from disk via the live `Filesystem`) and writes
/// back into the buffer. The op then commits the whole buffer atomically.
///
/// `BTreeMap` so the commit order is deterministic — replay applies
/// blocks in journal-stored order, matching the kernel's expected
/// transaction layout.
pub(crate) struct BlockBuffer {
    pub dirty: BTreeMap<u64, Vec<u8>>,
    /// Uninit flags this buffer clears, and the descriptor flags they
    /// leave behind, held until the buffer is committed.
    ///
    /// They cannot be published earlier. Clearing a group's uninit flag
    /// is what tells later allocations its bitmap is real and may be
    /// read; if the commit then fails, the bitmap on disk is still the
    /// unspecified bytes the flag existed to license skipping, and a
    /// planner that trusted the flag would allocate out of them.
    pub uninit_cleared: BTreeMap<usize, u16>,
}

impl BlockBuffer {
    /// `block_size` is taken and not stored.
    ///
    /// It was a field nothing read — every block this buffer holds
    /// arrives already sized by the caller, so the buffer never needs
    /// to know. The parameter stays because twenty-four call sites pass
    /// it and it says at each one which filesystem's blocks these are;
    /// dropping it would trade a dead field for twenty-four edits and a
    /// less legible call.
    pub fn new(_block_size: u32) -> Self {
        Self {
            dirty: BTreeMap::new(),
            uninit_cleared: BTreeMap::new(),
        }
    }

    /// Fetch a mutable handle to `block`, loading from `fs` on first
    /// touch. Subsequent calls for the same block return the in-buffer
    /// copy so multiple helpers can compose patches.
    pub fn get_mut(&mut self, fs: &Filesystem, block: u64) -> Result<&mut Vec<u8>> {
        if let std::collections::btree_map::Entry::Vacant(e) = self.dirty.entry(block) {
            let buf = fs.read_block(block)?;
            e.insert(buf);
        }
        Ok(self.dirty.get_mut(&block).unwrap())
    }

    /// Stage an already-built block image directly (no read-modify cycle).
    /// Useful when the caller has the bytes in hand (e.g. data blocks of
    /// a file write).
    pub fn put(&mut self, block: u64, bytes: Vec<u8>) {
        self.dirty.insert(block, bytes);
    }
}

/// Patch a split u32 counter (lo: u16 + optional hi: u16) in `buf` by `delta`.
///
/// ext4 BGD counters are stored as a 16-bit low word at `lo_off` and an
/// optional 16-bit high word at `hi_off` (present when desc_size >= 64). The
/// combined 32-bit value is clamped to zero on underflow.
fn patch_counter_u32(buf: &mut [u8], lo_off: usize, hi_off: Option<usize>, delta: i32) {
    let cur_lo = u16::from_le_bytes(buf[lo_off..lo_off + 2].try_into().unwrap()) as u32;
    let cur_hi = hi_off
        .map(|h| u16::from_le_bytes(buf[h..h + 2].try_into().unwrap()) as u32)
        .unwrap_or(0);
    let cur = (cur_hi << 16) | cur_lo;
    let new = (cur as i64 + delta as i64).clamp(0, u32::MAX as i64) as u32;
    buf[lo_off..lo_off + 2].copy_from_slice(&((new & 0xFFFF) as u16).to_le_bytes());
    if let Some(h) = hi_off {
        buf[h..h + 2].copy_from_slice(&(((new >> 16) & 0xFFFF) as u16).to_le_bytes());
    }
}

/// Apply counter deltas to the descriptor at `off` inside `block`: free
/// blocks, free inodes and used directories, each a u16 low half plus, when
/// `desc_size >= 64`, a u16 high half at the offsets `bgd::parse` reads.
fn patch_bgd_counter_fields(
    block: &mut [u8],
    off: usize,
    desc_size: u16,
    free_blocks_delta: i32,
    free_inodes_delta: i32,
    used_dirs_delta: i32,
) {
    use crate::bgd::{
        OFF_FREE_BLOCKS_HI, OFF_FREE_BLOCKS_LO, OFF_FREE_INODES_HI, OFF_FREE_INODES_LO,
        OFF_USED_DIRS_HI, OFF_USED_DIRS_LO,
    };
    let has_hi = desc_size >= 64;
    for (lo, hi, delta) in [
        (OFF_FREE_BLOCKS_LO, OFF_FREE_BLOCKS_HI, free_blocks_delta),
        (OFF_FREE_INODES_LO, OFF_FREE_INODES_HI, free_inodes_delta),
        (OFF_USED_DIRS_LO, OFF_USED_DIRS_HI, used_dirs_delta),
    ] {
        patch_counter_u32(block, off + lo, has_hi.then_some(off + hi), delta);
    }
}

/// Pack the low bits of an ext4 nanosecond timestamp field.
///
/// ext4 stores extra precision in a 32-bit extra field: bits [31:2] hold the
/// low 30 bits of the nanosecond value; bits [1:0] are the 2-bit epoch
/// extension that extends the 32-bit seconds counter beyond 2038.
#[inline]
fn pack_nsec_lo(nsec: u32) -> u32 {
    (nsec & 0x3FFF_FFFF) << 2
}

/// Passed to [`Filesystem::apply_utimens`] in place of a seconds value
/// to leave that timestamp unchanged — the equivalent of POSIX's
/// `UTIME_OMIT`, which `utimensat(2)` spells in the nanoseconds field.
///
/// `i64::MIN` and not `u32::MAX`: seconds are signed and 64-bit, so
/// `u32::MAX` is an ordinary date in 2106 and can no longer double as a
/// sentinel. `i64::MIN` is far outside anything ext4 can store.
pub const TIME_OMIT: i64 = i64::MIN;

/// Passed to [`Filesystem::apply_utimens`] as a nanoseconds value to set
/// that timestamp to the current time from the mount's
/// [`Runtime`](crate::runtime::Runtime); the seconds beside it are
/// ignored. The value `utimensat(2)` gives `UTIME_NOW` on Linux,
/// `(1 << 30) - 1`. The clock is whole seconds, so the stored
/// nanoseconds are zero.
pub const UTIME_NOW: u32 = (1 << 30) - 1;

/// Passed to [`Filesystem::apply_utimens`] as a nanoseconds value to
/// leave that timestamp unchanged; the seconds beside it are ignored.
/// The value `utimensat(2)` gives `UTIME_OMIT` on Linux, `(1 << 30) - 2`.
/// Equivalent to [`TIME_OMIT`] in the seconds.
pub const UTIME_OMIT: u32 = (1 << 30) - 2;

/// One more than the largest nanosecond count a timestamp can carry.
const NSEC_PER_SEC: u32 = 1_000_000_000;

/// What one `(sec, nsec)` pair passed to `apply_utimens` asks for.
#[derive(Clone, Copy)]
enum TimeUpdate {
    Omit,
    Set(i64, u32),
}

impl TimeUpdate {
    /// Resolve the `utimensat(2)` sentinels and refuse anything that is
    /// not a storable time, before any write.
    fn resolve(sec: i64, nsec: u32, now: i64) -> Result<Self> {
        if sec == TIME_OMIT || nsec == UTIME_OMIT {
            return Ok(Self::Omit);
        }
        if nsec == UTIME_NOW {
            return Ok(Self::Set(now, 0));
        }
        if nsec >= NSEC_PER_SEC {
            return Err(Error::InvalidArgument(
                "nanoseconds must be below 1e9 (or UTIME_NOW / UTIME_OMIT)",
            ));
        }
        if !(crate::inode::MIN_ENCODABLE_TIME..=crate::inode::MAX_ENCODABLE_TIME).contains(&sec) {
            return Err(Error::InvalidArgument(
                "timestamp outside the range ext4 can store (1901..2446)",
            ));
        }
        Ok(Self::Set(sec, nsec))
    }

    /// [`resolve`](Self::resolve)'s refusals alone, without a clock: the
    /// time `UTIME_NOW` stands for is always storable.
    fn check(sec: i64, nsec: u32) -> Result<()> {
        Self::resolve(sec, nsec, 0).map(|_| ())
    }
}

/// An inode named by number, for the inode-addressed entry points
/// (`lookup_at`, `stat_ino`, `apply_*_at`, `apply_*_ino`).
///
/// A handle-based host holds one of these per item instead of a path: a
/// path is the wrong key for a driver, because a hard link gives one inode
/// several, and renaming a directory changes every one beneath it.
///
/// `generation` is the `i_generation` the caller read alongside the number
/// (every attribute read returns it). With it, a handle to an inode that
/// was freed and then reused for a different file is refused as
/// [`Error::Stale`] instead of reaching the new file. Without it (`None`,
/// or a bare `u32` converted with `into()`) only a freed or never-used
/// inode is refused, which is what a caller that has not read the
/// generation yet can ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InodeRef {
    /// Inode number (1-based).
    pub ino: u32,
    /// Expected `i_generation`, or `None` to accept any.
    pub generation: Option<u32>,
}

impl InodeRef {
    /// Inode `ino`, which must still carry `generation`.
    pub const fn new(ino: u32, generation: u32) -> Self {
        Self {
            ino,
            generation: Some(generation),
        }
    }

    /// Inode `ino`, whatever its generation.
    pub const fn any(ino: u32) -> Self {
        Self {
            ino,
            generation: None,
        }
    }
}

impl From<u32> for InodeRef {
    fn from(ino: u32) -> Self {
        Self::any(ino)
    }
}

/// Refuse a name that cannot be one directory entry: empty, or holding a
/// `/` or a NUL. A path-addressed call never reaches these (its split
/// cannot produce them); an inode-addressed one is handed the name as-is.
fn check_entry_name(name: &[u8]) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidArgument("empty name"));
    }
    if name.contains(&b'/') {
        return Err(Error::InvalidArgument("a name cannot contain '/'"));
    }
    // See `split_parent_and_base`: the format would store it, e2fsck
    // reports it, and the kernel never files one.
    if name.contains(&0) {
        return Err(Error::InvalidArgument("a name cannot contain a NUL byte"));
    }
    Ok(())
}

/// [`check_entry_name`] for a name about to be filed, which must also fit
/// the entry's one-byte length.
fn check_new_entry_name(name: &[u8]) -> Result<()> {
    check_entry_name(name)?;
    if name.len() > 255 {
        return Err(Error::NameTooLong);
    }
    Ok(())
}

/// `.` and `..` name the directory and its parent, not entries that can
/// be removed or moved; rmdir(2) and rename(2) refuse them.
fn is_dot_or_dotdot(name: &[u8]) -> bool {
    name == b"." || name == b".."
}

/// Split a `/a/b/c` path into (`/a/b`, `c`). Returns an error for empty or
/// `"/"` paths (no basename to act on).
///
/// Bytes, like the names they end in: a path is never decoded (#418).
fn split_parent_and_base(path: &[u8]) -> Result<(&[u8], &[u8])> {
    let end = path.iter().rposition(|&b| b != b'/').map_or(0, |i| i + 1);
    let trimmed = &path[..end];
    if trimmed.is_empty() {
        return Err(Error::InvalidArgument("empty path"));
    }
    let last_slash = trimmed
        .iter()
        .rposition(|&b| b == b'/')
        .ok_or(Error::InvalidArgument("relative path"))?;
    let base = &trimmed[last_slash + 1..];
    let parent: &[u8] = if last_slash == 0 {
        b"/"
    } else {
        &trimmed[..last_slash]
    };
    if base.is_empty() {
        // Trailing slash on a non-dir path is POSIX ENOTDIR, not a generic arg error.
        return Err(Error::NotADirectory);
    }
    // A `&str` can hold NUL, and the entry format would store it: names are
    // counted bytes. The kernel never files one, because its names arrive
    // as C strings, and e2fsck reports one as an illegal character. Every
    // operation that files a name splits it here.
    if base.contains(&0) {
        return Err(Error::InvalidArgument("a name cannot contain a NUL byte"));
    }
    Ok((parent, base))
}

/// `DeepReader` adapter that pulls extent-tree internal/leaf node blocks
/// straight from a `Filesystem`'s underlying device (which at mount time
/// is wrapped in a `CachedDevice`, so reads benefit from the buffer cache
/// holding post-commit pre-checkpoint journaled writes).
///
/// Used by `apply_pwrite` to satisfy `plan_insert_extent_deep`'s
/// `&dyn DeepReader` argument when the inline extent root overflows and
/// the tree needs to be promoted to depth ≥ 1.
pub(crate) struct FsBlockReader<'a> {
    pub(crate) fs: &'a Filesystem,
}

impl<'a> crate::extent_mut::DeepReader for FsBlockReader<'a> {
    fn read_block(&self, block: u64, out: &mut [u8]) -> Result<()> {
        let bytes = self.fs.read_block(block)?;
        if bytes.len() != out.len() {
            return Err(Error::Corrupt(
                "FsBlockReader: block length mismatch (callers must pass a buffer sized to fs block_size)",
            ));
        }
        out.copy_from_slice(&bytes);
        Ok(())
    }
}

/// Extent-tree node blocks one call has planned but not yet committed, by
/// block number, laid over what the device holds.
///
/// A pwrite that promotes or splits its tree goes on planning against the
/// tree it has just changed: a later sub-run's insert descends into the
/// nodes the earlier one wrote, and each lookup walks them. Those nodes live
/// in the transaction's buffer and nowhere else until it commits (#389).
/// Writing them to the device early, so a plain reader could find them, put
/// rewrites of the file's EXISTING nodes on disk ahead of the bitmap bits,
/// counters and `i_blocks` that justify them: a call that then failed — out
/// of space on a later sub-run, a failed commit, a power cut — left the tree
/// mapping blocks the bitmap calls free, and bypassed the journal.
struct StagedTreeNodes<'a> {
    fs: &'a Filesystem,
    nodes: &'a std::collections::BTreeMap<u64, Vec<u8>>,
}

impl crate::extent_mut::DeepReader for StagedTreeNodes<'_> {
    fn read_block(&self, block: u64, out: &mut [u8]) -> Result<()> {
        match self.nodes.get(&block) {
            Some(bytes) if bytes.len() == out.len() => {
                out.copy_from_slice(bytes);
                Ok(())
            }
            Some(_) => Err(Error::Corrupt(
                "StagedTreeNodes: block length mismatch (callers must pass a buffer sized to fs block_size)",
            )),
            None => FsBlockReader { fs: self.fs }.read_block(block, out),
        }
    }
}

impl crate::block_io::BlockDevice for StagedTreeNodes<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.fs.dev.read_at(offset, buf)?;
        let bs = self.fs.sb.block_size() as u64;
        let end = offset + buf.len() as u64;
        for (&block, bytes) in self.nodes.range(offset / bs..end.div_ceil(bs)) {
            let (lo, hi) = ((block * bs).max(offset), ((block + 1) * bs).min(end));
            if lo < hi {
                buf[(lo - offset) as usize..(hi - offset) as usize].copy_from_slice(
                    &bytes[(lo - block * bs) as usize..(hi - block * bs) as usize],
                );
            }
        }
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.fs.dev.size_bytes()
    }
}

/// `EXT4_CASEFOLD_FL`: names in this directory hash casefolded.
const EXT4_CASEFOLD_FL: u32 = 0x4000_0000;
/// `EXT4_ENCRYPT_FL`: names in this directory are stored encrypted.
const EXT4_ENCRYPT_FL: u32 = 0x0000_0800;

/// Write atime, ctime, mtime (and crtime when the inode buffer is large
/// enough) from `now` into the raw inode bytes.
///
/// Each goes through [`set_inode_time`], so a time past 2038 keeps its
/// epoch bits where the inode has `*_extra` fields and is clamped where
/// it has none. Call after [`write_inode_extra_isize`]: whether those
/// fields exist is read from `i_extra_isize`.
fn write_inode_timestamps(raw: &mut [u8], now: i64) {
    set_inode_time(raw, InodeTime::Atime, now);
    set_inode_time(raw, InodeTime::Ctime, now);
    set_inode_time(raw, InodeTime::Mtime, now);
    // i_crtime (birth time) only exists in the extra section. Without it,
    // a birth-time reader shows 1970-01-01.
    set_inode_time(raw, InodeTime::Crtime, now);
}

/// Write a pre-allocated generation value into the raw inode bytes.
fn write_inode_generation(raw: &mut [u8], generation: u32) {
    use crate::inode::OFF_GENERATION;
    raw[OFF_GENERATION..OFF_GENERATION + 4].copy_from_slice(&generation.to_le_bytes());
}

/// Write i_extra_isize = 32 when the inode buffer is large enough.
/// 32 covers checksum_hi, nsec timestamps, and i_crtime beyond the 128-byte base.
fn write_inode_extra_isize(raw: &mut [u8]) {
    use crate::inode::{EXTRA_ISIZE_DEFAULT, INODE_SIZE_WITH_EXTRA, OFF_EXTRA_ISIZE};
    if raw.len() >= INODE_SIZE_WITH_EXTRA {
        raw[OFF_EXTRA_ISIZE..OFF_EXTRA_ISIZE + 2]
            .copy_from_slice(&EXTRA_ISIZE_DEFAULT.to_le_bytes());
    }
}

/// The uninit flags a mount has cleared since it read its descriptors, and
/// the descriptors with those flags applied.
///
/// One lock holds both, and every change to `cleared` goes through a method
/// here that drops `overridden` with it, so the cached vector cannot outlive
/// the state it was built from.
#[derive(Default)]
struct AllocationState {
    /// Group index to the descriptor flags its clear left behind.
    cleared: HashMap<usize, u16>,
    /// `Filesystem::groups` with `cleared` applied. Built on first use and
    /// shared by every read after it, until the next change.
    overridden: Option<Arc<[BlockGroupDescriptor]>>,
}

impl AllocationState {
    /// Take in the clears a committed buffer staged.
    fn publish(&mut self, cleared: BTreeMap<usize, u16>) {
        if cleared.is_empty() {
            return;
        }
        for (gi, flags) in cleared {
            self.cleared
                .entry(gi)
                .and_modify(|f| *f &= flags)
                .or_insert(flags);
        }
        self.overridden = None;
    }

    /// Forget every clear: the descriptors were read again and carry them.
    fn reset(&mut self) {
        self.cleared.clear();
        self.overridden = None;
    }
}

/// The descriptors the allocators plan against: the mount-time snapshot
/// itself, or the cached copy with this mount's cleared flags applied.
pub(crate) enum AllocationGroups<'a> {
    Snapshot(&'a [BlockGroupDescriptor]),
    Overridden(Arc<[BlockGroupDescriptor]>),
}

impl std::ops::Deref for AllocationGroups<'_> {
    type Target = [BlockGroupDescriptor];
    fn deref(&self) -> &[BlockGroupDescriptor] {
        match self {
            Self::Snapshot(groups) => groups,
            Self::Overridden(groups) => groups,
        }
    }
}

pub struct Filesystem {
    runtime: Arc<dyn crate::runtime::Runtime>,
    managed_recovery: bool,
    /// The buffer cache `dev` routes through, kept typed for its statistics.
    cache: Arc<crate::block_cache::CachedDevice>,
    pub dev: Arc<dyn BlockDevice>,
    pub sb: Superblock,
    pub groups: Vec<BlockGroupDescriptor>,
    /// Uninit flags this mount has already taken down on disk, by group.
    ///
    /// `groups` is a snapshot read once at mount and every write path holds
    /// `&self`, so the snapshot cannot be corrected in place when a group's
    /// INODE_UNINIT / BLOCK_UNINIT is cleared. That matters because the
    /// allocators *plan* against those flags: a group still flagged uninit is
    /// treated as entirely free without the bitmap being read at all. Left
    /// stale, the second allocation into a freshly-woken group hands back the
    /// very inode or block the first one just took — in the same mount, not
    /// merely the next one.
    ///
    /// Read through [`Filesystem::allocation_groups`], which is what the
    /// planners must be given.
    uninit_cleared: Mutex<AllocationState>,
    pub csum: Checksummer,
    /// Dialect detected at mount time from the superblock's feature flags.
    /// Drives runtime dispatch where ext2 / ext3 / ext4 differ — most
    /// notably the inode block-mapping scheme (extent vs indirect) used
    /// when allocating new inodes.
    pub flavor: features::FsFlavor,
    /// Live-write journal writer, present iff the FS has a journal AND
    /// the device is writable. `None` for read-only mounts and for ext2-
    /// style images. Locked per-op so mutating capi calls serialize on
    /// the JBD2 sequence cursor.
    pub journal: Option<std::sync::Mutex<crate::journal_writer::JournalWriter>>,
    /// Whether this mount has cleared `EXT4_VALID_FS` on disk, and so
    /// owes the superblock its state back when it is dropped (#85).
    marked_not_clean: std::sync::atomic::AtomicBool,
    /// The `s_state` found on disk when this mount cleared `EXT4_VALID_FS`:
    /// what [`Drop`] puts back. Kept apart from `sb`, which is re-read after
    /// replay and orphan recovery and by then holds the cleared state.
    state_found: std::sync::atomic::AtomicU16,
    /// Set when a direct (unjournaled) commit fails part-way (#319).
    ///
    /// Some of its blocks reached the disk and some did not, and the mount's
    /// own view -- `uninit_cleared` above all -- was never told about any of
    /// them. Planning another allocation against that view can hand out a
    /// block the disk already records as taken, so the mount stops writing:
    /// [`Self::refuse_write`] answers `ReadOnly` from here on, and [`Drop`]
    /// leaves the volume marked not clean for the checker.
    direct_commit_failed: std::sync::atomic::AtomicBool,
    /// The on-disk journal holds committed transactions this mount has not
    /// replayed onto the device (#375): a lazy mount, or one that mounted
    /// read-only and replayed into the cache alone.
    ///
    /// A commit writes its descriptor at the head of the log and then marks
    /// the journal clean, so one made now would overwrite the unreplayed
    /// transactions and discard them. [`Self::refuse_write`] answers
    /// [`Error::JournalNotReplayed`] until
    /// [`Self::replay_journal_if_dirty`] has put them on the device.
    replay_pending: std::sync::atomic::AtomicBool,
}

/// A mount that cleared `EXT4_VALID_FS` puts the state it found back when
/// it is dropped, which is this crate's unmount (`fs_ext4_umount` drops the
/// handle). A volume that was not clean when mounted stays not clean. Best
/// effort: a drop cannot report an error, and a device that refuses the
/// write leaves the volume marked not clean, which errs the safe way.
impl Drop for Filesystem {
    fn drop(&mut self) {
        // A journal whose commit failed mid-write leaves the device to the
        // next owner's replay: nothing more is written through this handle.
        let journal_failed = self
            .journal
            .as_ref()
            .is_some_and(|w| w.lock().map_or(true, |w| !w.is_healthy()));
        if self
            .marked_not_clean
            .load(std::sync::atomic::Ordering::SeqCst)
            && self.dev.is_writable()
            && !journal_failed
            && !self
                .direct_commit_failed
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            let state = self.state_found.load(std::sync::atomic::Ordering::SeqCst);
            let _ = self.write_superblock_state(state);
        }
    }
}

/// Encapsulates the common setup for creating a new inode in a directory:
/// resolved parent, pre-allocated inode number, and a `BlockBuffer` with the
/// inode-bitmap + BGD + SB counter updates already staged. Produced by
/// `Filesystem::plan_new_inode_in_dir`.
struct NewInodePlan {
    /// Newly allocated inode number (1-based).
    new_ino: u32,
    /// Inode number of the parent directory.
    parent_ino: u32,
    /// Parsed parent inode (for reading the directory block).
    parent_inode: crate::inode::Inode,
    /// Staged write buffer (bitmap + counter deltas already applied).
    buf: BlockBuffer,
}

/// Which BGD "uninit" flag a bitmap-marking call is about — see
/// `Filesystem::clear_bgd_uninit_flag_if_set`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BgdUninitFlag {
    Inode,
    Block,
}

/// The most bytes `apply_pwrite` buffers for one transaction. A chunk is
/// held several times over while it commits (block buffer, transaction,
/// serialised journal blocks); this bounds that, independent of the
/// journal's size. 32 MiB is at most 32768 blocks, the longest initialised
/// extent, at every block size, so no allocation within a chunk can outgrow
/// one extent.
const PWRITE_CHUNK_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Clean blocks the buffer cache of a [`Filesystem::mount`] keeps: about
/// 1 MiB at 4 KiB blocks. `docs/read-path-cost.md` records what it buys on a
/// measured tree (#68).
pub const DEFAULT_CACHE_BLOCKS: usize = 256;

impl Filesystem {
    /// Open an exclusively owned, backed-up device with checked journal recovery.
    /// Opening this writable handle is already a mutation: retain an external
    /// backup before calling. Only plain JBD2 (no transaction checksums, fast
    /// commit or async commit) is currently qualified by this lifecycle.
    pub fn mount_recovering(dev: Arc<dyn BlockDevice>) -> Result<Self> {
        let mut fs = Self::mount_lazy(dev)?;
        // This lifecycle replays the journal itself, below, with the live
        // writer detached until it has: nothing commits over the log first.
        fs.replay_pending
            .store(false, std::sync::atomic::Ordering::SeqCst);
        fs.refuse_write()?;
        let jsb = crate::jbd2::read_superblock(&fs)?.ok_or(Error::Unsupported(
            "checked recovery requires an internal journal",
        ))?;
        jsb.validate_plain_recovery(fs.sb.block_size(), fs.sb.blocks_count)?;
        if fs.sb.state & crate::superblock::EXT4_ERROR_FS != 0 {
            return Err(Error::Corrupt("filesystem records an outstanding error"));
        }
        fs.journal = None;
        fs.set_recovery_marker(true)?;
        crate::journal_apply::replay_if_dirty(&fs)?;
        fs.refresh_metadata()?;
        fs.refuse_write()?;
        crate::jbd2::read_superblock(&fs)?
            .ok_or(Error::Corrupt("journal disappeared"))?
            .validate_plain_recovery(fs.sb.block_size(), fs.sb.blocks_count)?;
        fs.set_recovery_marker(true)?;
        fs.journal = crate::journal_writer::JournalWriter::open(&fs)?
            .map(|w| Mutex::new(w.holding_needs_recovery()));
        fs.recover_orphans()?;
        fs.refresh_metadata()?;
        fs.managed_recovery = true;
        Ok(fs)
    }

    /// Finish a checked mount, propagating flush errors. Dropping a handle does
    /// not claim a clean release: the recovery marker remains for the next owner.
    /// After any I/O failure, release ownership and reopen rather than reusing it.
    pub fn finish(mut self) -> Result<()> {
        self.flush()?;
        if self.managed_recovery {
            self.refresh_metadata()?;
            if self.sb.last_orphan != 0 {
                return Err(Error::Corrupt("orphan recovery remains incomplete"));
            }
            // The marker is the release's commit point, so it is cleared
            // last: a failure before it leaves the volume still flagged
            // for recovery rather than claiming a release that did not
            // finish (#299).
            self.restore_state_found()?;
            return self.set_recovery_marker(false);
        }
        self.restore_state_found()
    }

    /// Put back the `s_state` this mount found, as [`Drop`] would, but with
    /// its error reported: `finish` is a release that says whether it
    /// happened, and the drop that follows it then has nothing left to write.
    fn restore_state_found(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        if !self.marked_not_clean.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.write_superblock_state(self.state_found.load(Ordering::SeqCst))?;
        self.marked_not_clean.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Flush a live mount without clearing its recovery marker or releasing it.
    /// Checked journal transactions must already be fully checkpointed. After an
    /// error the owner must retire the mount, just as for a failed mutation.
    ///
    /// Takes `&self` so a handle shared between threads can flush while other
    /// calls run. The journal writer is held for the whole check, so a
    /// transaction another thread is committing either finishes first or
    /// starts after: the check never sees one half-done.
    pub fn flush(&self) -> Result<()> {
        if self
            .direct_commit_failed
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(Error::Corrupt(
                "direct commit failed; reopen and check the volume",
            ));
        }
        let _writer = match &self.journal {
            Some(writer) => {
                let guard = writer
                    .lock()
                    .map_err(|_| Error::Corrupt("journal writer poisoned"))?;
                if !guard.is_healthy() {
                    return Err(Error::Corrupt(
                        "journal operation failed; reopen for recovery",
                    ));
                }
                Some(guard)
            }
            None => None,
        };
        self.dev.flush()?;
        if !self.managed_recovery {
            return Ok(());
        }
        let jsb =
            crate::jbd2::read_superblock(self)?.ok_or(Error::Corrupt("journal disappeared"))?;
        if !jsb.is_clean() || jsb.errno != 0 {
            return Err(Error::Corrupt("journal is not checkpointed"));
        }
        Ok(())
    }

    /// How many blocks the buffer cache holds pinned: bytes not yet at their
    /// final location on the device, which no capacity bound can evict. A
    /// read-only mount that replayed a dirty journal pins what it replayed;
    /// a writable mount checkpoints every commit, so it pins nothing.
    pub fn cache_pinned_blocks(&self) -> usize {
        self.cache.pinned_blocks()
    }

    /// Flush and discard checkpointed read caches before physical readback.
    /// The mount remains owned and usable. This is not concurrent-writer support:
    /// callers must serialize all filesystem access and retire on I/O failure.
    pub fn fresh_read(&mut self) -> Result<()> {
        self.flush()?;
        // Unmanaged writable mounts also use the immediate-checkpoint writer.
        // Pinned blocks are released only once the on-disk journal is proven
        // checkpointed. Without a writer nothing was checkpointed: a read-only
        // mount's replayed blocks exist only in the cache, so they stay pinned
        // and `invalidate_cache` refuses rather than serve pre-replay bytes.
        if self.journal.is_some() {
            let jsb =
                crate::jbd2::read_superblock(self)?.ok_or(Error::Corrupt("journal disappeared"))?;
            if !jsb.is_clean() || jsb.errno != 0 {
                return Err(Error::Corrupt("journal is not checkpointed"));
            }
            self.dev.unpin_all();
        }
        self.dev.invalidate_cache()?;
        self.refresh_metadata()
    }

    fn refresh_metadata(&mut self) -> Result<()> {
        let sb = Superblock::read(self.dev.as_ref())?;
        if sb.block_size() != self.sb.block_size()
            || sb.blocks_count != self.sb.blocks_count
            || sb.uuid != self.sb.uuid
            || sb.raw[0xd0..0xe8] != self.sb.raw[0xd0..0xe8]
        {
            return Err(Error::Corrupt(
                "journal changed filesystem identity or geometry",
            ));
        }
        features::check_mountable(sb.feature_incompat, sb.feature_ro_compat)?;
        let csum = Checksummer::from_superblock(&sb);
        if csum.enabled && !csum.verify_superblock(&sb.raw) {
            return Err(Error::BadChecksum {
                what: "replayed superblock",
            });
        }
        let groups = bgd::read_all(self.dev.as_ref(), &sb, &csum)?;
        self.flavor = features::FsFlavor::detect(sb.feature_compat, sb.feature_incompat);
        self.sb = sb;
        self.groups = groups;
        self.csum = csum;
        self.uninit_cleared
            .lock()
            .map_err(|_| Error::Corrupt("allocation state poisoned"))?
            .reset();
        Ok(())
    }

    fn set_recovery_marker(&mut self, needed: bool) -> Result<()> {
        let recover = features::Incompat::RECOVER.bits();
        let on_disk = Superblock::read(self.dev.as_ref())?.feature_incompat & recover != 0;
        if on_disk != needed {
            crate::journal_apply::write_needs_recovery(self.dev.as_ref(), needed)?;
            self.dev.flush()?;
        }
        self.refresh_metadata()
    }

    /// Mount the ext4 filesystem on `dev`. Read-only unless the device reports
    /// `is_writable()`, in which case a dirty journal is replayed before
    /// returning so callers see a consistent on-disk state.
    ///
    /// When `RO_COMPAT_METADATA_CSUM` is set, the superblock checksum is
    /// verified — failure aborts the mount with `Error::BadChecksum`.
    pub fn mount(dev: Arc<dyn BlockDevice>) -> Result<Self> {
        Self::mount_inner(
            dev,
            false,
            DEFAULT_CACHE_BLOCKS,
            Arc::new(crate::runtime::SystemRuntime),
        )
    }

    /// [`Filesystem::mount`] with a buffer cache of `blocks` clean blocks
    /// rather than [`DEFAULT_CACHE_BLOCKS`]. Zero keeps none, so every read
    /// reaches the device: the baseline `tests/read_path_cost.rs` measures the
    /// cache against (#68). Journaled blocks awaiting checkpoint are held
    /// whatever the capacity.
    pub fn mount_with_cache(dev: Arc<dyn BlockDevice>, blocks: usize) -> Result<Self> {
        Self::mount_inner(dev, false, blocks, Arc::new(crate::runtime::SystemRuntime))
    }

    /// Like `mount`, but skips the mount-time journal replay even when the
    /// device is writable. The caller is responsible for invoking
    /// [`Filesystem::replay_journal_if_dirty`] once the underlying write
    /// path is actually ready to service writes (e.g. in the FSKit case the
    /// kernel-level write FD on `FSBlockDeviceResource` only becomes
    /// writable AFTER `loadResource` returns successfully — replaying mid-
    /// `loadResource` produces EIO).
    ///
    /// Until replay runs, reads observe the on-disk pre-replay state and
    /// any write through this handle fails with
    /// [`Error::JournalNotReplayed`] while the journal still says dirty
    /// (#375): a commit would overwrite the unreplayed log. This is the lazy/deferred-replay sibling of `mount`; for
    /// most callers `mount` is correct.
    pub fn mount_lazy(dev: Arc<dyn BlockDevice>) -> Result<Self> {
        Self::mount_inner(
            dev,
            true,
            DEFAULT_CACHE_BLOCKS,
            Arc::new(crate::runtime::SystemRuntime),
        )
    }

    /// Mount with caller-provided wall time and inode-generation policy.
    /// The provider must remain valid for the entire mount, including recovery.
    pub fn mount_with_runtime(
        dev: Arc<dyn BlockDevice>,
        runtime: Arc<dyn crate::runtime::Runtime>,
    ) -> Result<Self> {
        Self::mount_inner(dev, false, DEFAULT_CACHE_BLOCKS, runtime)
    }

    fn mount_inner(
        dev: Arc<dyn BlockDevice>,
        defer_replay: bool,
        cache_blocks: usize,
        runtime: Arc<dyn crate::runtime::Runtime>,
    ) -> Result<Self> {
        let sb = Superblock::read(dev.as_ref())?;
        features::check_mountable(sb.feature_incompat, sb.feature_ro_compat)?;
        let flavor = features::FsFlavor::detect(sb.feature_compat, sb.feature_incompat);
        let csum = Checksummer::from_superblock(&sb);
        if csum.enabled && !csum.verify_superblock(&sb.raw) {
            return Err(Error::BadChecksum { what: "superblock" });
        }
        let groups = bgd::read_all(dev.as_ref(), &sb, &csum)?;
        sb.check_fits_device(dev.size_bytes())?;
        // Wrap the raw device in a write-through buffer cache. All
        // reads and writes for the rest of this mount session route
        // through the cache. A read-only mount that replays a dirty
        // journal pins the replayed blocks here, since they never reach
        // the device. The clean capacity is `DEFAULT_CACHE_BLOCKS` unless
        // the caller chose; pinned entries are unbounded until journal
        // replay calls `unpin_all`.
        let cache = Arc::new(crate::block_cache::CachedDevice::new(
            dev,
            sb.block_size(),
            cache_blocks,
        ));
        let dev: Arc<dyn BlockDevice> = cache.clone();
        let mut fs = Self {
            runtime,
            managed_recovery: false,
            cache,
            dev,
            sb,
            groups,
            uninit_cleared: Mutex::new(AllocationState::default()),
            csum,
            flavor,
            journal: None,
            marked_not_clean: std::sync::atomic::AtomicBool::new(false),
            direct_commit_failed: std::sync::atomic::AtomicBool::new(false),
            replay_pending: std::sync::atomic::AtomicBool::new(false),
            state_found: std::sync::atomic::AtomicU16::new(0),
        };

        // Replay a dirty journal: onto the device if it is writable (below,
        // after the write-breaking check), into the cache if it is not. A
        // dirty journal's transactions are committed, not pending, and a
        // read-only view that skips them reads superseded metadata (#72).
        // A lazy mount defers only the writes; reading the journal is no
        // different from the rest of mount.
        //
        // Both the walker (`journal_block_to_physical`) and the writer
        // (`JournalWriter::open`) now dispatch on `indirect::map_logical_any`,
        // so ext3 (whose journal inode uses legacy indirect block pointers)
        // works the same as ext4 (extent tree). The Phase A blanket refusal
        // of ext3 RW is therefore lifted.
        // MMP — Multi-Mount Protection — exists to stop two hosts
        // mounting one filesystem read-write at the same time and
        // destroying it. Honouring it means reading the MMP block,
        // checking its sequence, writing our own node name, waiting,
        // and re-checking; none of that is implemented.
        //
        // Ignoring the bit is defensible for a read-only mount: a
        // reader cannot corrupt anything, and the other host's
        // protection is unaffected. It is NOT defensible the moment we
        // are the one writing -- which this crate does, through
        // twenty-one apply_* entry points and a live journal writer,
        // both reached below on exactly this condition.
        //
        // So the refusal is scoped to the writable case. A read-only
        // mount of an MMP filesystem still works, which is what a user
        // recovering data from a disk another machine has open
        // actually wants.
        //
        // CASEFOLD has the same shape and is refused by the same check —
        // see `features::WRITE_BREAKING_INCOMPAT`, which is where the
        // reasoning for each bit now lives. It is one set rather than a
        // chain of `if`s because a second copy of this shape is how the
        // first one gets forgotten.
        let write_breaking = crate::features::write_breaking_incompat(fs.sb.feature_incompat);
        if fs.dev.is_writable() && write_breaking != 0 {
            return Err(crate::error::Error::UnsupportedIncompat(write_breaking));
        }

        let replayed = if !fs.dev.is_writable() {
            crate::journal_apply::replay_into_cache(&fs)?
        } else if !defer_replay {
            // A replay failure fails the mount: the error surfaces so the
            // caller can decide whether to retry or proceed; we fail loud
            // rather than silent.
            crate::journal_apply::replay_if_dirty(&fs)?
        } else {
            0
        };
        if replayed > 0 {
            fs.reload_geometry()?;
        }

        // Open the live-write journal writer once replay is done. Any
        // pending transactions are now applied; the writer can take over
        // the JBD2 cursor from a clean state. Returns None when there is
        // no journal at all (ext2), so the if-let handles every flavor
        // uniformly.
        //
        // GATED ON `refuse_write` RATHER THAN ON THE DEVICE, so a volume
        // carrying a feature this driver does not maintain does not get
        // a live-write journal it will never be allowed to use.
        //
        // Journal REPLAY above is deliberately not gated the same way.
        // Replay is a write, but it is the one write that adds nothing:
        // it finishes transactions the filesystem itself already
        // committed, including whatever the last writer did to the
        // structures behind the unmaintained bit. Refusing it would
        // leave a volume that reads its own stale metadata, which is a
        // worse answer than the one it prevents.
        if fs.write_refusal().is_ok() {
            if let Some(jw) = crate::journal_writer::JournalWriter::open(&fs)? {
                fs.journal = Some(std::sync::Mutex::new(jw));
            }
        }

        // A journal still dirty on disk after the replay decision above --
        // deferred, or replayed into the cache of a read-only device -- keeps
        // every write waiting for `replay_journal_if_dirty` (#375). Set after
        // the writer is opened, whose own gate is `write_refusal`.
        if !fs.dev.is_writable() || defer_replay {
            let dirty = crate::jbd2::read_superblock(&fs)?.is_some_and(|j| !j.is_clean());
            fs.replay_pending
                .store(dirty, std::sync::atomic::Ordering::SeqCst);
        }

        // Phase 6.2 — orphan recovery. Runs after journal replay so any
        // pending kernel-level transactions have already played back;
        // any inode still on the orphan chain at this point is genuinely
        // dead or awaiting completion of a truncate. Failure aborts the mount;
        // never discard an error then clear the orphan list anyway.
        if !defer_replay {
            // `recover_orphans` consults `refuse_write` itself and
            // returns zero when it must not write.
            fs.recover_orphans()?;
            fs.refresh_metadata()?;
        }

        Ok(fs)
    }

    /// Read the superblock and group descriptors again, through the cache.
    /// Mount read both before replaying the journal, which may carry newer
    /// copies of either (#72).
    fn reload_geometry(&mut self) -> Result<()> {
        let sb = Superblock::read(self.dev.as_ref())?;
        features::check_mountable(sb.feature_incompat, sb.feature_ro_compat)?;
        let csum = Checksummer::from_superblock(&sb);
        if csum.enabled && !csum.verify_superblock(&sb.raw) {
            return Err(Error::BadChecksum { what: "superblock" });
        }
        self.groups = bgd::read_all(self.dev.as_ref(), &sb, &csum)?;
        sb.check_fits_device(self.dev.size_bytes())?;
        // The cached override was built from the descriptors just replaced.
        self.uninit_cleared
            .get_mut()
            .map_err(|_| Error::Corrupt("allocation state poisoned"))?
            .overridden = None;
        self.flavor = features::FsFlavor::detect(sb.feature_compat, sb.feature_incompat);
        self.csum = csum;
        self.sb = sb;
        Ok(())
    }

    /// Run journal replay now if the journal is dirty. Idempotent — calling
    /// this on a clean (or read-only) volume is a no-op that returns 0.
    /// Designed to pair with [`Filesystem::mount_lazy`], but safe to call
    /// on any handle.
    /// Refuse a write this driver cannot make consistently.
    ///
    /// Two reasons, and they are different in kind:
    ///
    /// - the device cannot be written at all;
    /// - the volume carries a `RO_COMPAT` feature bit describing
    ///   structures a write here would leave behind.
    ///
    /// The second is what the bit is FOR, and it was enforced nowhere.
    /// `check_mountable` decides what may be read -- its own comment
    /// says "mounted read-only" -- and this driver stopped being
    /// read-only long ago. So an unrecognised bit permitted reading,
    /// which is correct, and writing, which is the failure the bit
    /// exists to prevent: the write updates what this driver knows
    /// about and silently leaves the rest, and nothing reports it.
    ///
    /// `QUOTA` is the case to picture. It is tolerated for reading and
    /// nothing here maintains the quota inodes, so a create charged
    /// nobody for the file and left counters describing a filesystem
    /// that no longer exists.
    pub(crate) fn refuse_write(&self) -> Result<()> {
        self.write_refusal()?;
        self.mark_not_clean_once()
    }

    /// [`Self::refuse_write`]'s verdict alone, without marking the volume
    /// not clean: for the mount path, which asks whether it could write
    /// before it knows whether it will.
    fn write_refusal(&self) -> Result<()> {
        if !self.dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        // A direct commit that failed part-way left the disk and this
        // mount's view of it disagreeing (#319): no further write can be
        // planned safely, as the kernel remounts read-only on a write error.
        if self
            .direct_commit_failed
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(Error::ReadOnly);
        }
        if self
            .replay_pending
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(Error::JournalNotReplayed);
        }
        let unmaintained = features::unmaintained_ro_compat(self.sb.feature_ro_compat);
        if unmaintained != 0 {
            return Err(Error::UnsupportedRoCompat(unmaintained));
        }
        // THE MOUNT'S INCOMPAT REFUSAL, AGAIN, HERE. It runs once, against
        // `is_writable()` at mount time, and writability is not fixed: a
        // lazy mount starts read-only and the host hands it a write FD
        // afterwards (see `mount_lazy`). That mount passed the check by
        // being read-only and then wrote to a CASEFOLD or MMP volume
        // (#117). The mount-time check stays, so a volume that is writable
        // from the start still fails to mount rather than mounting and
        // refusing.
        let write_breaking = features::write_breaking_incompat(self.sb.feature_incompat);
        if write_breaking != 0 {
            return Err(Error::UnsupportedIncompat(write_breaking));
        }
        Ok(())
    }

    /// Clear `EXT4_VALID_FS` in the on-disk superblock before this mount's
    /// first write, as the kernel does when it mounts read-write (#85).
    ///
    /// The bit is what `fsck -p` at boot, and `e2fsck`'s "clean", read as
    /// "cleanly unmounted". This driver never touched it, so a volume a
    /// crash interrupted mid-write still claimed to have been put away
    /// properly and the boot-time check skipped it. Cleared here, it stays
    /// cleared until [`Drop`] puts the mount-time state back; a crash in
    /// between leaves it clear, which is the point.
    ///
    /// Done on the first write rather than at mount so a read-write mount
    /// that never writes leaves the volume byte-for-byte as it found it.
    fn mark_not_clean_once(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        if self.marked_not_clean.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.state_found.store(self.sb.state, Ordering::SeqCst);
        self.write_superblock_state(self.sb.state & !crate::superblock::EXT4_VALID_FS)?;
        self.marked_not_clean.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Write `state` into the on-disk superblock's `s_state` (0x3A),
    /// directly rather than through a transaction -- the kernel's own
    /// mount and unmount writes are direct -- keeping its checksum right.
    fn write_superblock_state(&self, state: u16) -> Result<()> {
        let at = crate::superblock::SUPERBLOCK_OFFSET;
        let mut sb = vec![0u8; 1024];
        self.dev.read_at(at, &mut sb)?;
        sb[0x3A..0x3C].copy_from_slice(&state.to_le_bytes());
        if self.csum.enabled {
            let csum = crate::checksum::linux_crc32c(!0, &sb[..0x3FC]);
            sb[0x3FC..0x400].copy_from_slice(&csum.to_le_bytes());
        }
        self.dev.write_at(at, &sb)?;
        self.dev.flush()
    }

    ///
    /// Takes `&mut self` because a replay that applied anything leaves the
    /// mount's own view behind (#376): the superblock and descriptors were
    /// read before it, and a journal writer opened over the dirty log holds
    /// its pre-replay sequence. All three are brought up to date here, as
    /// an eager mount does before it opens its writer.
    pub fn replay_journal_if_dirty(&mut self) -> Result<usize> {
        let dirty = self.dev.is_writable()
            && crate::jbd2::read_superblock(self)?.is_some_and(|j| !j.is_clean());
        let n = crate::journal_apply::replay_if_dirty(self)?;
        // `replay_if_dirty` does nothing on a device that cannot be written,
        // so only a writable one has had its log put on disk.
        if self.dev.is_writable() {
            self.replay_pending
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        // Replay applied every pending journaled write to the data area,
        // so the device-layer cache's "pinned" entries (post-commit but
        // pre-checkpoint) are now consistent with disk. Tell the cache
        // it can stop pinning them — future evictions are safe.
        // Skip when nothing replayed: a clean journal returns 0, and
        // unpinning here would demote pinned-but-still-needed entries
        // from a live handle's prior journaled writes, letting later
        // cache misses serve stale data-area bytes.
        if n > 0 {
            self.dev.unpin_all();
            // The replayed transactions may carry newer copies of the
            // superblock and descriptors. Planning against the copies read
            // at mount treats a group whose UNINIT flag the replay cleared
            // as untouched, synthesises its bitmap, and hands out what the
            // replay allocated. `uninit_cleared` described the old copies.
            self.reload_geometry()?;
            // `reset`, not a bare clear: the allocation state is a
            // struct since #341 and also caches the descriptors with the
            // clears applied, which must go with them.
            self.uninit_cleared
                .lock()
                .map_err(|_| Error::Corrupt("allocation state poisoned"))?
                .reset();
        }
        // The writer takes over the log cursor from the replayed journal
        // superblock, as mount's does, so its next commit follows the
        // replayed sequence rather than rewinding it. One opened over the
        // dirty log is replaced; a mount that was read-only when it mounted
        // has none, and is given one now that the device can take writes.
        // A healthy writer over a journal that was already clean is left
        // alone: it owns the cursor.
        // A writer whose commit failed is kept: its failure is what `flush`
        // and `Drop` report, and a fresh one would hide it.
        let failed = self
            .journal
            .as_ref()
            .is_some_and(|w| w.lock().map_or(true, |w| !w.is_healthy()));
        if (dirty || self.journal.is_none()) && !failed && self.write_refusal().is_ok() {
            self.journal = crate::journal_writer::JournalWriter::open(self)?.map(Mutex::new);
        }
        Ok(n)
    }

    /// Phase 6.1 — walk the orphan inode chain rooted at `s_last_orphan`
    /// and return its members in chain order.
    ///
    /// The chain has TWO kinds of member, and they are not distinguished
    /// here — see [`Filesystem::recover_orphans`], which branches on
    /// `i_links_count` the way `ext4_orphan_cleanup` does. An inode with
    /// no links is an unlink-while-open: its blocks and its inode should
    /// be reclaimed. An inode that still has links is a `truncate()` a
    /// crash interrupted: it is still named by its directory entries and
    /// only the blocks past `i_size` should go.
    ///
    /// The chain is encoded by overloading `i_dtime` as "next orphan
    /// inode number"; the chain terminates when `dtime == 0`. We cap at
    /// `inodes_count` to avoid runaway loops on cycle-corrupted images.
    ///
    /// Read-only (no recovery yet — that's Phase 6.2). Returns `Ok([])`
    /// when there are no orphans.
    pub fn orphan_list(&self) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut cur = self.sb.last_orphan;
        let cap = self.sb.inodes_count;
        let mut steps = 0u32;
        while cur != 0 {
            if steps > cap {
                return Err(Error::Corrupt(
                    "orphan_list: chain longer than inodes_count (cycle?)",
                ));
            }
            out.push(cur);
            // Read the inode's i_dtime (offset 0x14..0x18) to find the
            // next link. Don't go through read_inode_verified because an
            // orphan inode's checksum may be stale by design.
            let raw = self.read_inode_raw(cur)?;
            if raw.len() < 0x18 {
                return Err(Error::Corrupt("orphan_list: inode too short"));
            }
            cur = u32::from_le_bytes(raw[0x14..0x18].try_into().unwrap());
            steps += 1;
        }
        Ok(out)
    }

    /// The orphan chain as recovery walks it: [`Self::orphan_list`], except
    /// that it stops at a member that is not allocated in the inode bitmap
    /// and whose `i_dtime` is no longer a link, as the kernel's
    /// `ext4_orphan_cleanup` stops at a "bad orphan inode" before clearing
    /// `s_last_orphan`.
    ///
    /// Such a member is an orphan an earlier recovery already reclaimed and
    /// stamped with a deletion time. Following that time as an inode number
    /// failed every later mount's recovery on "InvalidInode", and the chain
    /// was never cleared (#124).
    ///
    /// A free member whose `i_dtime` is still an inode number is different:
    /// a recovery was cut while reclaiming it, before the superblock moved
    /// the head past it. [`Self::recover_orphans`] keeps that link until the
    /// head has moved, so the walk goes through it and on to the members
    /// behind it, and the retry finishes whatever the cut left.
    fn orphan_chain_to_recover(&self) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut cur = self.sb.last_orphan;
        while cur != 0 {
            if out.len() as u64 > u64::from(self.sb.inodes_count) {
                return Err(Error::Corrupt(
                    "orphan_list: chain longer than inodes_count (cycle?)",
                ));
            }
            let allocated = self.inode_bit_is_set(cur)?;
            if !allocated && (cur > self.sb.inodes_count || cur < self.sb.first_inode) {
                break;
            }
            let raw = self.read_inode_raw(cur)?;
            if raw.len() < 0x18 {
                return Err(Error::Corrupt("orphan_list: inode too short"));
            }
            let next = u32::from_le_bytes(raw[0x14..0x18].try_into().unwrap());
            if !allocated && next > self.sb.inodes_count {
                break;
            }
            out.push(cur);
            cur = next;
        }
        Ok(out)
    }

    /// Whether `ino` is marked in use in its group's inode bitmap. An
    /// out-of-range number, or a group still `INODE_UNINIT`, is not.
    fn inode_bit_is_set(&self, ino: u32) -> Result<bool> {
        if ino == 0 || ino > self.sb.inodes_count || self.sb.inodes_per_group == 0 {
            return Ok(false);
        }
        let ipg = self.sb.inodes_per_group;
        let groups = self.allocation_groups();
        let Some(group) = groups.get(((ino - 1) / ipg) as usize) else {
            return Ok(false);
        };
        if group.flags().contains(crate::bgd::BgdFlags::INODE_UNINIT) {
            return Ok(false);
        }
        let bitmap = self.read_block(group.inode_bitmap)?;
        let bit = ((ino - 1) % ipg) as usize;
        Ok(bitmap
            .get(bit / 8)
            .is_some_and(|byte| byte & (1 << (bit % 8)) != 0))
    }

    /// Phase 6.2 — orphan replay.
    ///
    /// # THE CHAIN HAS TWO KINDS OF MEMBER AND THEY GET OPPOSITE
    /// TREATMENT
    ///
    /// The kernel branches on `i_links_count` in `ext4_orphan_cleanup`,
    /// and so does this:
    ///
    /// - **No links** — an unlink-while-open. Nothing names it any more,
    ///   so its data blocks and its inode-bitmap slot are freed and its
    ///   body is zeroed with `i_dtime = now`.
    /// - **Links remaining** — a `truncate()` that a crash interrupted.
    ///   `i_size` was already lowered before the machine went down; the
    ///   blocks past it were not yet freed. The file is still named by
    ///   its directory entries, so recovery FINISHES THE TRUNCATE and
    ///   leaves the file in place: free what lies past `i_size`, rewrite
    ///   the extent root and `i_blocks`, clear `i_dtime`, and touch
    ///   nothing else.
    ///
    /// Treating the second kind as the first is what this used to do,
    /// and it destroyed data: the inode of a file the user never deleted
    /// was freed and its body — including the block pointers that were
    /// the only way back to its contents — zeroed, while the directory
    /// entries naming it were left pointing at a free inode.
    ///
    /// # ONE COMMIT PER MEMBER, AND THE LINK OUTLIVES THE MEMBER
    ///
    /// A member's only link to the next is its own `i_dtime`, which both
    /// treatments overwrite. So each member is its own commit, which also
    /// moves `s_last_orphan` on to the next member, and in that commit the
    /// member's `i_dtime` still holds the link. Without a journal the
    /// superblock is written last, so a cut anywhere in the commit leaves
    /// the head on this member with the link intact, and the next mount
    /// walks through it to the rest (see [`Self::orphan_chain_to_recover`]).
    /// The final `i_dtime` (a deletion time, or zero for a finished
    /// truncate) is written in the NEXT member's commit, once the head is
    /// durably past it. The last member has nothing behind it to lose and
    /// gets its final `i_dtime` at once.
    ///
    /// All in one commit, a cut after the head's group and before a later
    /// member's left the head free with a deletion time and the later
    /// member allocated, and the next mount cleared the chain at the head
    /// and stranded the rest (Greptile on #201). A cut between two commits
    /// can still leave one member off the chain with its link for an
    /// `i_dtime`, which `e2fsck -p` resets without asking.
    ///
    /// Returns the number of orphan inodes **reclaimed** — completed
    /// truncates are not counted, because nothing was reclaimed. No-op
    /// (returns 0) when the chain is empty or the device is read-only.
    ///
    /// Designed to be called from the mount path AFTER journal replay,
    /// so the orphans we're about to reclaim are guaranteed not still in
    /// use by an in-flight kernel-level transaction.
    pub fn recover_orphans(&self) -> Result<usize> {
        // NOT AN ERROR HERE, unlike the other write paths. This runs
        // from the mount path on every mount, and a volume that cannot
        // be written -- or that this driver must not write, because it
        // carries a feature it does not maintain -- simply keeps its
        // orphans. Failing the mount over it would refuse a volume that
        // reads perfectly well.
        if self.write_refusal().is_err() {
            return Ok(0);
        }
        if self.sb.last_orphan == 0 {
            return Ok(0);
        }
        // There is work, and it writes.
        self.mark_not_clean_once()?;
        let chain = self.orphan_chain_to_recover()?;

        let bs = self.sb.block_size();
        let mut reclaimed = 0usize;
        // The member the previous commit moved the head past, and the
        // `i_dtime` it should end with.
        let mut unstamped: Option<(u32, u32)> = None;

        if chain.is_empty() {
            let mut buf = BlockBuffer::new(bs);
            self.buffer_patch_sb_last_orphan(&mut buf, 0)?;
            return self.commit_block_buffer(buf).map(|()| 0);
        }

        for (at, &orphan_ino) in chain.iter().enumerate() {
            let next = chain.get(at + 1).copied().unwrap_or(0);
            let mut buf = BlockBuffer::new(bs);
            if let Some((ino, dtime)) = unstamped.take() {
                self.buffer_stamp_dtime(&mut buf, ino, dtime)?;
            }
            let Some((blocks, final_dtime, deleted)) =
                self.buffer_recover_orphan(&mut buf, orphan_ino, next)?
            else {
                // AN ORPHAN THIS CANNOT RECLAIM STAYS AT THE HEAD (CodeRabbit
                // on #201). Its inode and blocks are left untouched and the
                // chain is not advanced past it, so nothing is freed without
                // its blocks and nothing leaves the chain still holding them;
                // the members behind it wait with it. Only the previous
                // member's pending `i_dtime` is written.
                self.commit_block_buffer(buf)?;
                break;
            };
            self.buffer_patch_sb_counters(&mut buf, blocks as i64, i32::from(deleted))?;
            reclaimed += usize::from(deleted);
            if next == 0 {
                self.buffer_stamp_dtime(&mut buf, orphan_ino, final_dtime)?;
            } else {
                unstamped = Some((orphan_ino, final_dtime));
            }
            self.buffer_patch_sb_last_orphan(&mut buf, next)?;
            self.commit_block_buffer(buf)?;
        }
        Ok(reclaimed)
    }

    /// Set `ino`'s `i_dtime` in `buf`, keeping its checksum right.
    fn buffer_stamp_dtime(&self, buf: &mut BlockBuffer, ino: u32, dtime: u32) -> Result<()> {
        let (block, offset) = bgd::locate_inode(&self.sb, &self.groups, ino)?;
        let inode_size = self.sb.inode_size as usize;
        let bytes = buf.get_mut(self, block)?;
        let raw = bytes
            .get_mut(offset as usize..offset as usize + inode_size)
            .ok_or(Error::Corrupt("inode slice exceeds block data"))?;
        raw[0x14..0x18].copy_from_slice(&dtime.to_le_bytes());
        let generation = u32::from_le_bytes(raw[0x64..0x68].try_into().unwrap());
        let mut owned = raw.to_vec();
        self.finalize_inode_raw(ino, generation, &mut owned)?;
        raw.copy_from_slice(&owned);
        Ok(())
    }

    /// Reclaim one chain member into `buf`, leaving `link` in its
    /// `i_dtime`. Returns the blocks freed, the `i_dtime` the member ends
    /// with, and whether the inode itself was freed; `None`, having touched
    /// nothing, for a member this cannot reclaim whole -- one that does not
    /// parse, one whose indirect tree cannot be read or points outside the
    /// filesystem, or one whose extent tree the truncate planner does not
    /// handle.
    fn buffer_recover_orphan(
        &self,
        buf: &mut BlockBuffer,
        orphan_ino: u32,
        link: u32,
    ) -> Result<Option<(u64, u32, bool)>> {
        let bs = self.sb.block_size();
        // Read the orphan's raw bytes (skip csum verify — orphan
        // inodes routinely carry stale csums by design).
        let mut raw = self.read_inode_raw(orphan_ino)?;
        let parsed = match Inode::parse(&raw) {
            Ok(i) => i,
            Err(_) => return Ok(None), // unparseable orphan — skip + leak rather than panic
        };

        // STILL NAMED BY A DIRECTORY. This is an interrupted
        // truncate, not a deletion. Finish the truncate and leave
        // the file alone.
        if parsed.links_count != 0 {
            let Some(freed) =
                self.buffer_finish_interrupted_truncate(buf, orphan_ino, &parsed, raw)?
            else {
                return Ok(None);
            };
            self.buffer_stamp_dtime(buf, orphan_ino, link)?;
            return Ok(Some((freed, 0, false)));
        }

        let mut freed = 0u64;
        // LEGACY INDIRECT: every ext2 and ext3 file. Its data blocks and
        // the indirect blocks that map them are freed with it, as far as
        // its size reaches -- the bound the kernel's truncate uses -- in
        // this same transaction. This used to free the inode alone, which
        // left all of those blocks allocated and named by nothing (#79).
        //
        // Only an inode whose i_block holds pointers, decided as unlink
        // decides it: a fast symlink's target or an inline file's data is
        // no block map, and `i_blocks` also counts an xattr block, so
        // `blocks > 0` alone freed the blocks their text named (#384).
        if !parsed.has_extents() && Self::holds_block_map(&parsed, bs) {
            let data_blocks = u32::try_from(parsed.size.div_ceil(bs as u64))
                .map_err(|_| Error::Corrupt("orphan: indirect file too large to map"))?;
            let Ok(tree) = crate::indirect_mut::collect_for_free(
                &parsed.block,
                bs,
                data_blocks,
                self.dev.as_ref(),
            ) else {
                return Ok(None);
            };
            let fs_blocks = self.sb.blocks_count;
            let in_range = |start: u64, len: u64| {
                start >= self.sb.first_data_block as u64
                    && start.checked_add(len).is_some_and(|end| end <= fs_blocks)
            };
            // A pointer outside the filesystem is a corrupt tree; freeing
            // around it would clear bits that belong to nothing. Leave the
            // member whole, as for any member this cannot reclaim.
            if !tree
                .data_runs
                .iter()
                .all(|r| in_range(r.start, r.len as u64))
                || !tree.indirect_blocks.iter().all(|&b| in_range(b, 1))
            {
                return Ok(None);
            }
            for run in &tree.data_runs {
                freed += self.buffer_free_block_run_and_bgd(buf, run.start, run.len as u64)?;
            }
            for &block in &tree.indirect_blocks {
                freed += self.buffer_free_block_run_and_bgd(buf, block, 1)?;
            }
        }
        if parsed.has_extents() {
            let Ok(runs) = self.extent_tree_runs(orphan_ino, &parsed) else {
                return Ok(None);
            };
            freed += self.buffer_free_runs(buf, &runs)?;
        }
        // Its xattr block: freed, or one reference fewer if shared.
        if parsed.file_acl != 0 {
            freed += self.buffer_release_xattr_block(buf, parsed.file_acl)?;
        }
        // Free the inode bitmap slot + BGD free_inodes++.
        self.buffer_free_inode_slot(buf, orphan_ino)?;

        // Zero the inode body (preserve generation). i_blocks is moot
        // once zeroed; the freed extents are in the counters above.
        let inode_size = self.sb.inode_size as usize;
        let old_gen = parsed.generation;
        for b in &mut raw[..inode_size] {
            *b = 0;
        }
        raw[0x14..0x18].copy_from_slice(&link.to_le_bytes());
        raw[0x64..0x68].copy_from_slice(&old_gen.to_le_bytes());
        self.finalize_inode_raw(orphan_ino, old_gen, &mut raw)?;
        self.buffer_write_inode(buf, orphan_ino, &raw)?;
        Ok(Some((freed, self.dtime_now(), true)))
    }

    /// Finish a `truncate()` that a crash interrupted, for an orphan that
    /// still has directory links.
    ///
    /// `i_size` was lowered before the machine went down and is therefore
    /// already the size the user asked for; what is left over is the
    /// blocks past it. So this frees exactly those, rewrites the extent
    /// root and `i_blocks` to match, and clears `i_dtime` — which was
    /// doing double duty as the orphan chain's "next" pointer, and which
    /// a live file must not carry, because a non-zero `i_dtime` is how
    /// every other tool reads "this inode was deleted".
    ///
    /// Returns the number of blocks freed, to be added to the
    /// superblock's free count by the caller's single transaction.
    ///
    /// # WHEN THE TRUNCATE CANNOT BE PLANNED, THE FILE IS STILL LEFT
    /// INTACT
    ///
    /// Legacy indirect mappings still cannot be planned here: leave their
    /// blocks allocated instead of deleting a named file. An extent tree
    /// the planner refuses -- a node that fails its checksum or does not
    /// describe a valid tree -- returns `None` having staged nothing, and
    /// the caller keeps the member at the head of the chain: corrupt
    /// metadata is neither freed around nor dropped from the orphan list.
    fn buffer_finish_interrupted_truncate(
        &self,
        buf: &mut BlockBuffer,
        ino: u32,
        parsed: &Inode,
        mut raw: Vec<u8>,
    ) -> Result<Option<u64>> {
        let bs = self.sb.block_size() as u64;
        let mut freed_blocks: u64 = 0;
        let mut freed_sectors: u64 = 0;

        if parsed.has_extents() {
            // old == new: `plan_truncate_shrink` works from the logical
            // end that `new_size` implies, so passing i_size for both
            // frees precisely what lies past the file's declared end.
            {
                // Planning reads and verifies every node and stages
                // nothing, so a refusal here leaves `buf` as it was.
                let Ok((_sc, muts)) = self.plan_inode_truncate(ino, parsed, parsed.size) else {
                    return Ok(None);
                };
                for m in &muts {
                    match m {
                        crate::extent_mut::ExtentMutation::WriteRoot { bytes } => {
                            Self::patch_inode_block_area(&mut raw, bytes)?;
                        }
                        crate::extent_mut::ExtentMutation::WriteTreeBlock { block, bytes } => {
                            buf.put(*block, bytes.clone());
                        }
                        crate::extent_mut::ExtentMutation::FreePhysicalRun { start, len } => {
                            freed_blocks +=
                                self.buffer_free_block_run_and_bgd(buf, *start, *len as u64)?;
                            freed_sectors += (*len as u64) * (bs / 512);
                        }
                        _ => {
                            return Err(Error::Corrupt(
                                "orphan truncate: unexpected mutation type",
                            ));
                        }
                    }
                }
                let new_blocks = parsed.blocks.saturating_sub(freed_sectors);
                Self::patch_inode_size_and_blocks(&mut raw, parsed.size, new_blocks)?;
            }
        }

        // Off the chain, and no longer looking deleted. This happens on
        // every path through here, including the ones that freed nothing,
        // because an inode with links and a non-zero `i_dtime` is a
        // contradiction that outlives the mount.
        raw[0x14..0x18].copy_from_slice(&0u32.to_le_bytes());
        self.finalize_inode_raw(ino, parsed.generation, &mut raw)?;
        self.buffer_write_inode(buf, ino, &raw)?;
        Ok(Some(freed_blocks))
    }

    /// Read a whole block by its logical block number. Routes through
    /// `self.dev`, which at mount time is wrapped in a `CachedDevice` —
    /// so this single call benefits from the buffer cache that holds
    /// post-commit, pre-checkpoint journaled writes.
    pub fn read_block(&self, block_num: u64) -> Result<Vec<u8>> {
        let block_size = self.sb.block_size() as usize;
        let byte_offset = block_num
            .checked_mul(block_size as u64)
            .ok_or(Error::Corrupt("block byte offset overflow"))?;
        let mut buf = vec![0u8; block_size];
        self.dev.read_at(byte_offset, &mut buf)?;
        Ok(buf)
    }

    /// Read raw inode bytes for a given inode number (does not parse).
    pub fn read_inode_raw(&self, ino: u32) -> Result<Vec<u8>> {
        let (block, offset) = bgd::locate_inode(&self.sb, &self.groups, ino)?;
        let block_data = self.read_block(block)?;
        let inode_size = self.sb.inode_size as usize;
        let off = offset as usize;
        let end = off
            .checked_add(inode_size)
            .ok_or(Error::Corrupt("inode slice end overflows usize"))?;
        if end > block_data.len() {
            return Err(Error::Corrupt("inode slice exceeds block data"));
        }
        Ok(block_data[off..end].to_vec())
    }

    /// Read + parse + checksum-verify an inode in one shot.
    ///
    /// When `RO_COMPAT_METADATA_CSUM` is enabled the inode CRC32C is checked
    /// (salted by inode number + generation per ext4 spec). A mismatch
    /// returns `Error::BadChecksum { what: "inode" }`.
    pub fn read_inode_verified(&self, ino: u32) -> Result<(Inode, Vec<u8>)> {
        let raw = self.read_inode_raw(ino)?;
        let inode = Inode::parse(&raw)?;
        if self.csum.enabled && !self.csum.verify_inode(ino, inode.generation, &raw) {
            return Err(Error::BadChecksum { what: "inode" });
        }
        // A DIRECTORY IS NOT SPARSE.
        //
        // Every directory scan in this crate walks
        // `0..size.div_ceil(block_size)` and steps over a logical block
        // that is not mapped -- which is what the kernel does too, so
        // the loop is never ended by an error and never bounded by real
        // content. `i_size` is `join32(i_size_high, i_size_lo)` off the
        // disk: setting `i_size_high` on the root of a small image gave
        // a directory of 2^44 bytes and a lookup that was still
        // spinning after twenty seconds, with `MAX_DIR_ENTRIES` never
        // reached because no entry is ever found.
        //
        // A regular file may legitimately declare more bytes than the
        // filesystem holds -- that is what a sparse file is -- but a
        // directory's blocks are all really there.
        if inode.is_dir() && inode.size > self.byte_ceiling()? {
            return Err(Error::Corrupt(
                "directory inode declares more bytes than the filesystem holds",
            ));
        }
        Ok((inode, raw))
    }

    /// The target of the symlink at `ino`, exactly as stored and without
    /// a terminating NUL.
    ///
    /// A target shorter than `i_block` (60 bytes) is a fast symlink, held
    /// inline in `i_block`; a longer one is a slow symlink, held in data
    /// blocks. That is the boundary the kernel's `ext4_symlink` writes.
    ///
    /// `Error::InvalidArgument` when the inode is not a symlink,
    /// `Error::Corrupt` when it declares a target longer than any path,
    /// and `Error::Unsupported` when the target is encrypted.
    pub fn read_link(&self, ino: u32) -> Result<Vec<u8>> {
        const I_BLOCK_BYTES: u64 = 60;
        // No path is longer than PATH_MAX. Without this the raw `i_size`
        // became an allocation: `i_mode = 0xA1FF` with
        // `i_size = 0x2000_0000_0000_0060` aborted the process.
        const PATH_MAX: u64 = 4096;

        let (inode, _raw) = self.read_inode_verified(ino)?;
        if !inode.is_symlink() {
            return Err(Error::InvalidArgument("not a symlink"));
        }
        if inode.size > PATH_MAX {
            return Err(Error::Corrupt("symlink target is longer than any path"));
        }
        // A fast symlink's target is ciphertext too, and never reaches
        // file_io's refusal (#76).
        crate::file_io::refuse_encrypted(&inode)?;
        if inode.size < I_BLOCK_BYTES {
            return Ok(inode.block[..inode.size as usize].to_vec());
        }
        let mut out = vec![0u8; inode.size as usize];
        crate::file_io::read_verified(self, &inode, ino, 0, inode.size, &mut out)?;
        Ok(out)
    }

    /// The most bytes anything on this filesystem can really occupy:
    /// `blocks_count * block_size`, checked rather than saturated, and
    /// never more than the device (#321). Mount already refuses a
    /// superblock larger than its device; taking the minimum here as well
    /// keeps the bound honest for a caller that reaches these paths with
    /// a superblock mount did not vet.
    pub fn byte_ceiling(&self) -> Result<u64> {
        let bytes = self.sb.filesystem_bytes().ok_or(Error::Corrupt(
            "superblock: blocks_count * block_size overflows",
        ))?;
        Ok(bytes.min(self.dev.size_bytes()))
    }

    // ----------------------------------------------------------------------
    // Inode-addressed entry points (#372)
    // ----------------------------------------------------------------------
    //
    // Every path-addressed operation resolves its path and then calls one
    // of these; they are the implementation, and the path functions are a
    // resolve step in front of them. A handle-based host calls them
    // directly with the inode numbers it holds.

    /// Resolve `path` to an inode number, verifying directory blocks.
    ///
    /// A path is bytes, compared byte for byte against the entry names,
    /// which have no encoding: one that is not UTF-8 names exactly the
    /// file whose name it holds, and one naming nothing is
    /// [`Error::NotFound`]. Every path-addressed `apply_*` resolves through
    /// here; each `&str` form is its byte form with `str::as_bytes` (#418).
    pub fn lookup_path_bytes(&self, path: &[u8]) -> Result<u32> {
        let mut reader = |ino: u32| self.read_inode_verified(ino).map(|(i, _)| i);
        crate::path::lookup_bytes_with_csum(
            self.dev.as_ref(),
            &self.sb,
            &mut reader,
            path,
            &self.csum,
        )
    }

    fn resolve(&self, path: &[u8]) -> Result<u32> {
        self.lookup_path_bytes(path)
    }

    /// Resolve the parent of `path`, for an operation on its final name.
    fn resolve_parent<'p>(&self, path: &'p [u8]) -> Result<(u32, &'p [u8])> {
        let (parent, base) = split_parent_and_base(path)?;
        Ok((self.resolve(parent)?, base))
    }

    /// [`resolve_parent`](Self::resolve_parent) for an operation that files
    /// the final name, which is refused as too long before anything is
    /// resolved.
    fn resolve_new_parent<'p>(&self, path: &'p [u8]) -> Result<(u32, &'p [u8])> {
        let (parent, base) = split_parent_and_base(path)?;
        if base.len() > 255 {
            return Err(Error::NameTooLong);
        }
        Ok((self.resolve(parent)?, base))
    }

    /// Read the inode `r` names, refusing a handle that no longer names a
    /// file with [`Error::Stale`]: a number outside the inode table or in
    /// its reserved range (other than the root), an inode not marked in use
    /// in its bitmap or with no links or no mode (freed, or never used), or
    /// one whose generation is not the
    /// one `r` carries (freed and reused).
    pub(crate) fn live_inode(&self, r: InodeRef) -> Result<(Inode, Vec<u8>)> {
        let ino = r.ino;
        let reserved = ino < self.sb.first_inode && ino != crate::path::EXT4_ROOT_INODE;
        if ino == 0 || ino > self.sb.inodes_count || reserved {
            return Err(Error::Stale);
        }
        // Checked before the inode is read: a slot that was never used can
        // hold anything, including a checksum that does not verify.
        if !self.inode_bit_is_set(ino)? {
            return Err(Error::Stale);
        }
        let (inode, raw) = self.read_inode_verified(ino)?;
        if inode.links_count == 0 || inode.mode == 0 {
            return Err(Error::Stale);
        }
        if r.generation.is_some_and(|g| g != inode.generation) {
            return Err(Error::Stale);
        }
        Ok((inode, raw))
    }

    /// [`live_inode`](Self::live_inode) for a directory.
    fn live_dir(&self, r: InodeRef) -> Result<(Inode, Vec<u8>)> {
        let (inode, raw) = self.live_inode(r)?;
        if !inode.is_dir() {
            return Err(Error::NotADirectory);
        }
        Ok((inode, raw))
    }

    /// The attributes of the inode `r` names; [`Error::Stale`] if it no
    /// longer names a file (see [`InodeRef`]).
    pub fn stat_ino(&self, r: impl Into<InodeRef>) -> Result<Inode> {
        Ok(self.live_inode(r.into())?.0)
    }

    /// The inode number `name` has in directory `dir` — one step of a path
    /// walk. `name` is bytes, compared exactly, so a name that is not UTF-8
    /// is found. `.` and `..` are ordinary entries here.
    pub fn lookup_at(&self, dir: impl Into<InodeRef>, name: &[u8]) -> Result<u32> {
        check_entry_name(name)?;
        let dir = dir.into();
        let (dir_inode, _) = self.live_dir(dir)?;
        crate::path::find_entry(
            self.dev.as_ref(),
            &self.sb,
            dir.ino,
            &dir_inode,
            name,
            &self.csum,
        )
    }

    /// Every entry of directory `dir`, `.` and `..` included, in on-disk
    /// order. Directory listing by path resolves and calls this.
    pub fn read_dir_ino(&self, dir: impl Into<InodeRef>) -> Result<Vec<crate::dir::DirEntry>> {
        let dir = dir.into();
        let (inode, raw) = self.live_dir(dir)?;
        self.dir_entries(dir.ino, &inode, &raw)
    }

    /// Collect the entries of the directory `inode`, number `ino`, whose
    /// on-disk bytes are `inode_raw`.
    fn dir_entries(
        &self,
        ino: u32,
        inode: &Inode,
        inode_raw: &[u8],
    ) -> Result<Vec<crate::dir::DirEntry>> {
        use crate::dir::DirBlockIter;
        crate::file_io::refuse_encrypted_names(inode)?;
        let block_size = self.sb.block_size();
        let has_filetype = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;

        // Inline-data dirs (tiny dirs stored inside the inode itself).
        // Checked before the extent test: an inline directory has no extent
        // tree, and was refused as a legacy one. `.` and `..` synthesised,
        // entries from byte 4 and then from the `system.data` continuation
        // (#427).
        if inode.has_inline_data() {
            return crate::inline_data::read_dir(
                self.dev.as_ref(),
                ino,
                inode,
                inode_raw,
                self.sb.inode_size,
                block_size,
                has_filetype,
            );
        }
        if !inode.has_extents() {
            return Err(Error::Corrupt("legacy (non-extent) dirs not yet supported"));
        }

        // Bound on entries buffered per listing. A crafted image with
        // `inode.size` claiming gigabytes would otherwise allocate
        // proportionally, since the loop grows `entries` straight from
        // on-disk content.
        const MAX_DIR_ENTRIES: usize = 1_000_000;
        let mut entries = Vec::new();
        let push = |entries: &mut Vec<crate::dir::DirEntry>, e| {
            if entries.len() >= MAX_DIR_ENTRIES {
                return Err(Error::Corrupt("dir entries exceed MAX_DIR_ENTRIES"));
            }
            entries.push(e);
            Ok(())
        };

        let total_blocks = inode.size.div_ceil(block_size as u64);
        let mut block_buf = vec![0u8; block_size as usize];
        for logical in 0..total_blocks {
            let Some(phys) =
                crate::extent::map_logical(&inode.block, self.dev.as_ref(), block_size, logical)?
            else {
                continue; // sparse hole
            };
            self.dev.read_at(phys * block_size as u64, &mut block_buf)?;
            for entry in DirBlockIter::new(&block_buf, has_filetype) {
                push(&mut entries, entry?)?;
            }
        }
        Ok(entries)
    }

    /// Read up to `out.len()` bytes of the regular file `r` names, from
    /// byte `offset`. Returns the number of bytes read, short at the end
    /// of the file.
    pub fn read_ino(&self, r: impl Into<InodeRef>, offset: u64, out: &mut [u8]) -> Result<usize> {
        let r = r.into();
        let (inode, raw) = self.live_inode(r)?;
        if !inode.is_file() {
            return Err(Error::InvalidArgument("not a regular file"));
        }
        let length = (out.len() as u64).min(inode.size);
        crate::file_io::read_with_raw_verified(
            self,
            &inode,
            &raw,
            r.ino,
            offset,
            length,
            &mut out[..length as usize],
        )
        .map(|n| n as usize)
    }

    /// The target of the symlink `r` names; see [`read_link`](Self::read_link).
    pub fn read_link_ino(&self, r: impl Into<InodeRef>) -> Result<Vec<u8>> {
        let r = r.into();
        self.live_inode(r)?;
        self.read_link(r.ino)
    }

    /// Set the size of the regular file `r` names, freeing what a shrink
    /// drops; a grow is sparse. `Error::IsADirectory` for a directory and
    /// `Error::InvalidArgument` for any other non-regular file.
    pub fn apply_truncate_ino(&self, r: impl Into<InodeRef>, new_size: u64) -> Result<()> {
        self.refuse_write()?;
        let r = r.into();
        let (inode, _) = self.live_inode(r)?;
        // Truncating a directory frees its blocks and loses `.` and `..`;
        // POSIX truncate(2) says EISDIR. Symlinks and devices: EINVAL.
        if inode.is_dir() {
            return Err(Error::IsADirectory);
        }
        if !inode.is_file() {
            return Err(Error::InvalidArgument(
                "truncate target is not a regular file",
            ));
        }
        // At equality either works; grow only bumps timestamps.
        if new_size >= inode.size {
            self.apply_truncate_grow(r.ino, new_size)
        } else {
            self.apply_truncate_shrink(r.ino, new_size)
        }
    }

    /// Map a logical block within `inode` to its physical block, choosing
    /// between the extent tree and the legacy direct/indirect scheme based
    /// on `EXT4_EXTENTS_FL`. Returns `None` for sparse holes and (for the
    /// extent path) uninitialised extents — callers wanting zeros there
    /// must handle the `None` case explicitly.
    ///
    /// This is the per-inode dispatcher every directory traversal /
    /// extent-walking call site should use instead of touching
    /// `extent::map_logical` directly — without it, an ext2/3 inode with
    /// raw block pointers in `i_block` gets misparsed as an extent header
    /// (yielding `CorruptExtentTree("bad extent header magic")`).
    ///
    /// An inline-data inode has no block map, so it is refused with
    /// `Error::Unsupported` (#382): that is what makes every directory
    /// mutation of an inline directory fail before it writes anything.
    ///
    /// The indirect path internally maintains its own block cache for the
    /// duration of the call; sequential lookups via repeated calls don't
    /// share that cache (file_io's read paths build a longer-lived cache
    /// to amortize across blocks).
    pub fn map_inode_logical(&self, inode: &Inode, logical_block: u64) -> Result<Option<u64>> {
        // An inline-data inode's i_block holds bytes, not pointers: for a
        // directory, its parent's inode number and then entries; for a
        // file, its first 60 bytes. Read as a block map, they name blocks
        // belonging to something else, which every directory writer then
        // parsed and wrote through (#382).
        if inode.has_inline_data() {
            return Err(Error::Unsupported(
                "inline-data inode: i_block holds data, not a block map; \
                 writing inline-data directories is not supported",
            ));
        }
        let bs = self.sb.block_size();
        if (inode.flags & crate::inode::InodeFlags::EXTENTS.bits()) != 0 {
            crate::extent::map_logical(&inode.block, self.dev.as_ref(), bs, logical_block)
        } else {
            let mut cache = crate::indirect::IndirectCache::new();
            crate::indirect::lookup(
                &inode.block,
                self.dev.as_ref(),
                bs,
                logical_block,
                &mut cache,
            )
        }
    }

    /// Write the given raw inode bytes back to disk. Refused as every other
    /// write is (`refuse_write`): `Error::ReadOnly` on a read-only device,
    /// and a volume carrying a feature this driver must not write is left
    /// untouched. The first write marks the volume not clean (#323).
    ///
    /// **Not checksum-aware**: callers that update fields affecting the inode
    /// CRC32C (anything except `checksum_lo` / `checksum_hi`) must recompute
    /// + patch the checksum into `raw` before calling this. Not wrapped in a
    /// journal transaction — see E11 / `journal_apply` for the journaled
    /// version. Use only when the caller has the full write-ordering story
    /// under control.
    pub fn write_inode_raw(&self, ino: u32, raw: &[u8]) -> Result<()> {
        if raw.len() != self.sb.inode_size as usize {
            return Err(Error::Corrupt("write_inode_raw: length != inode_size"));
        }
        self.refuse_write()?;
        let (block, offset) = bgd::locate_inode(&self.sb, &self.groups, ino)?;
        let block_size = self.sb.block_size() as u64;
        let byte_offset = block * block_size + offset as u64;
        self.dev.write_at(byte_offset, raw)?;
        Ok(())
    }

    /// Write `i_file_acl` — the external xattr block pointer — into a raw
    /// inode. `block_nr` of 0 clears it.
    ///
    /// THE HIGH HALF IS AT 0x76, NOT 0x74. `Inode::parse` reads
    /// `i_file_acl_hi` from `0x76..0x78`; both writers used to put it at
    /// `0x74..0x76`, which is `l_i_blocks_hi`. This function now writes it
    /// once at `0x76..0x78`. Previously,
    /// `patch_inode_size_and_blocks` — which owns that field — ran six
    /// lines later at both sites and overwrote it. So the high half was
    /// never written and never cleared, by either of the two functions
    /// that thought they were maintaining it.
    ///
    /// WHY IT LEFT NO TRACE. Below 2^32 blocks the high half is 0, the
    /// clobber writes 0 over 0, and the field was already 0. Above it —
    /// 16 TiB at 4 KiB blocks — a fresh external block keeps only its low
    /// 32 bits, so `Inode::parse` reads back a DIFFERENT block, which
    /// `xattr::list` then reads and `apply_removexattr` WRITES; and a
    /// freed one leaves `file_acl == old_hi << 32` pointing at a block
    /// already handed back to the allocator.
    ///
    /// ONE RECIPE, TWO CALLERS, which is the other half of why this
    /// survived: the offset was written out by hand at each site and
    /// nothing made the two agree with the reader.
    ///
    /// The capacity check uses `0x78`, not `0x76`: the old guard admitted a
    /// buffer ending exactly where the field it was about to write begins.
    /// It runs before either half is written, so an inode too short to hold
    /// the high half is REFUSED without leaving a truncated pointer behind.
    pub(crate) fn write_file_acl(raw: &mut [u8], block_nr: u64) -> Result<()> {
        if raw.len() < 0x6C {
            return Err(Error::Corrupt(
                "write_file_acl: inode buffer too small for i_file_acl_lo",
            ));
        }
        let (hi, lo) = crate::extent_mut::split_phys_block(block_nr);
        if raw.len() < 0x78 && hi != 0 {
            return Err(Error::Corrupt(
                "write_file_acl: this inode is too small to hold i_file_acl_hi and the \
                 external xattr block needs it",
            ));
        }
        raw[0x68..0x6C].copy_from_slice(&lo.to_le_bytes());
        if raw.len() >= 0x78 {
            raw[0x76..0x78].copy_from_slice(&hi.to_le_bytes());
        }
        Ok(())
    }

    /// Patch fields in a raw inode image: size, blocks_count. Leaves all
    /// other bytes (including the extent tree header + entries in `i_block`)
    /// intact. `new_block_count` is in 512-byte sectors per spec (same
    /// convention as `Inode::blocks`).
    pub fn patch_inode_size_and_blocks(
        raw: &mut [u8],
        new_size: u64,
        new_block_count: u64,
    ) -> Result<()> {
        if raw.len() < 128 {
            return Err(Error::Corrupt("patch_inode: buffer too small"));
        }
        // size = size_lo (0x04..0x08) + size_hi (0x6C..0x70)
        let size_lo = (new_size & 0xFFFF_FFFF) as u32;
        let size_hi = (new_size >> 32) as u32;
        raw[0x04..0x08].copy_from_slice(&size_lo.to_le_bytes());
        raw[0x6C..0x70].copy_from_slice(&size_hi.to_le_bytes());
        // blocks = blocks_lo (0x1C..0x20, u32) + blocks_hi (0x74..0x76, u16)
        let blocks_lo = (new_block_count & 0xFFFF_FFFF) as u32;
        let blocks_hi = ((new_block_count >> 32) & 0xFFFF) as u16;
        raw[0x1C..0x20].copy_from_slice(&blocks_lo.to_le_bytes());
        raw[0x74..0x76].copy_from_slice(&blocks_hi.to_le_bytes());
        Ok(())
    }

    /// Overwrite the 60-byte `i_block` area of an inode image with `new_root`.
    /// Used when an extent-tree mutation changes the inline root.
    pub fn patch_inode_block_area(raw: &mut [u8], new_root: &[u8]) -> Result<()> {
        if raw.len() < 128 {
            return Err(Error::Corrupt("patch_inode_block_area: buffer too small"));
        }
        if new_root.len() != 60 {
            return Err(Error::Corrupt(
                "patch_inode_block_area: new_root != 60 bytes",
            ));
        }
        raw[0x28..0x64].copy_from_slice(new_root);
        Ok(())
    }

    /// Verify every external node before planning any tree/bitmap mutation.
    /// Freed tree nodes use the same block accounting as freed data blocks.
    fn plan_inode_truncate(
        &self,
        ino: u32,
        inode: &Inode,
        new_size: u64,
    ) -> Result<(
        crate::file_mut::SizeChange,
        Vec<crate::extent_mut::ExtentMutation>,
    )> {
        if !inode.has_extents() {
            return Err(Error::Unsupported("truncate of legacy block mappings"));
        }
        let mut read = |block| {
            let bytes = self.read_block(block)?;
            if !self.csum.verify_extent_tail(ino, inode.generation, &bytes) {
                return Err(Error::BadChecksum {
                    what: "extent block",
                });
            }
            Ok(bytes)
        };
        let (change, mut mutations) = crate::file_mut::plan_truncate_shrink_deep(
            inode.size,
            new_size,
            &inode.block,
            self.sb.block_size(),
            self.sb.blocks_count,
            &mut read,
        )?;
        for mutation in &mut mutations {
            if let crate::extent_mut::ExtentMutation::WriteTreeBlock { bytes, .. } = mutation {
                self.csum.patch_extent_tail(ino, inode.generation, bytes);
            }
        }
        Ok((change, mutations))
    }

    /// Shrink a file to `new_size`. Composes `file_mut::plan_truncate_shrink`
    /// (extent-tree updates + freed-block ranges) with actual disk writes —
    /// rewrites the inode and zeros the freed bitmap bits.
    ///
    /// The inode write, the bitmap writes, the BGD and the superblock
    /// accumulate into one `BlockBuffer` and commit together. On a mount
    /// with a journal that commit is one transaction, atomic with respect
    /// to a crash. Without one (an ext2 volume, or ext4 formatted without
    /// a journal) `commit_block_buffer` writes the blocks in turn, and a
    /// crash part-way leaves some written and some not (#179).
    ///
    /// This said "Not journaled … safe only in a test scratch image", and
    /// promised the transaction as future work. The future work landed;
    /// the warning outlived it and was steering callers away from an API
    /// that is safe.
    pub fn apply_truncate_shrink(&self, ino: u32, new_size: u64) -> Result<()> {
        self.refuse_write()?;
        let (inode, mut raw) = self.read_inode_verified(ino)?;
        Self::refuse_truncate_of(&inode)?;
        if new_size > inode.size {
            return Err(Error::InvalidArgument(
                "truncate: new_size > old_size (grow not supported)",
            ));
        }

        if new_size == inode.size {
            return Ok(());
        }
        let (_size_change, muts) = self.plan_inode_truncate(ino, &inode, new_size)?;

        let bs = self.sb.block_size() as u64;
        let mut freed_sectors: u64 = 0;
        let mut freed_blocks: u64 = 0;

        // Multi-block transaction: accumulate inode + bitmap + BGD + SB
        // mutations into one buffer, commit through the journal atomically.
        let mut buf = BlockBuffer::new(self.sb.block_size());

        for m in &muts {
            match m {
                crate::extent_mut::ExtentMutation::WriteRoot { bytes } => {
                    Self::patch_inode_block_area(&mut raw, bytes)?;
                }
                crate::extent_mut::ExtentMutation::WriteTreeBlock { block, bytes } => {
                    buf.put(*block, bytes.clone());
                }
                crate::extent_mut::ExtentMutation::FreePhysicalRun { start, len } => {
                    freed_blocks +=
                        self.buffer_free_block_run_and_bgd(&mut buf, *start, *len as u64)?;
                    freed_sectors += (*len as u64) * (bs / 512);
                }
                _ => {
                    return Err(Error::Corrupt(
                        "apply_truncate_shrink: unexpected mutation type",
                    ));
                }
            }
        }

        // Clear the retained partial block so a later grow cannot expose
        // bytes beyond the new EOF. Holes and unwritten extents need no write.
        let tail = (new_size % bs) as usize;
        if tail != 0 {
            if let Some(block) = self.map_inode_logical(&inode, new_size / bs)? {
                buf.get_mut(self, block)?[tail..].fill(0);
            }
        }
        // Patch size + blocks_count in the inode image, finalize csum.
        let new_blocks = inode.blocks.saturating_sub(freed_sectors);
        Self::patch_inode_size_and_blocks(&mut raw, new_size, new_blocks)?;
        self.finalize_inode_raw(ino, inode.generation, &mut raw)?;
        self.buffer_write_inode(&mut buf, ino, &raw)?;

        if freed_blocks > 0 {
            self.buffer_patch_sb_counters(&mut buf, freed_blocks as i64, 0)?;
        }

        self.commit_block_buffer(buf)
    }

    /// Extend a file to `new_size`. The new range is a sparse hole — ext4's
    /// extent tree treats unmapped logical blocks as zeros, so no extent
    /// mutation and no block allocation are required. Only `i_size`,
    /// `i_mtime`, `i_ctime`, and the inode checksum change.
    ///
    /// Caller (capi dispatch) guarantees `new_size >= inode.size`. If
    /// `new_size == inode.size` this is a no-op that still bumps the
    /// timestamps — matches `truncate(2)` semantics.
    /// Truncation is for regular files. A directory's size is its blocks, a
    /// symlink's is its target's length, and a device node has none; setting
    /// any of them to a size the caller chose leaves an inode e2fsck rejects.
    /// The kernel answers EISDIR for a directory and EINVAL for the rest.
    fn refuse_truncate_of(inode: &Inode) -> Result<()> {
        if inode.is_dir() {
            return Err(Error::IsADirectory);
        }
        if !inode.is_file() {
            return Err(Error::InvalidArgument(
                "truncate: only a regular file has a size to change",
            ));
        }
        Self::refuse_inline_data_write(inode)
    }

    /// Refuse a content write to an inline-data file (#383). Its `i_block`
    /// holds the file's first 60 bytes and `system.data` the rest, so
    /// neither writer applies: the extent path would parse the bytes as a
    /// tree, the block-map path frees them as pointers, and a size change
    /// alone leaves `i_size` past what the inline area holds. Until inline
    /// writes exist, `Unsupported` leaves the file whole.
    fn refuse_inline_data_write(inode: &Inode) -> Result<()> {
        if inode.has_inline_data() {
            return Err(Error::Unsupported(
                "writing the content of an inline-data file is not supported",
            ));
        }
        Ok(())
    }

    pub fn apply_truncate_grow(&self, ino: u32, new_size: u64) -> Result<()> {
        self.refuse_write()?;
        let (inode, mut raw) = self.read_inode_verified(ino)?;
        Self::refuse_truncate_of(&inode)?;
        if new_size < inode.size {
            return Err(Error::InvalidArgument(
                "apply_truncate_grow: new_size < old_size (use apply_truncate_shrink)",
            ));
        }
        Self::patch_inode_size_and_blocks(&mut raw, new_size, inode.blocks)?;

        let now = self.runtime.now_unix_seconds();
        set_inode_time(&mut raw, InodeTime::Ctime, now);
        set_inode_time(&mut raw, InodeTime::Mtime, now);

        self.finalize_inode_raw(ino, inode.generation, &mut raw)?;
        self.commit_inode_write(ino, &raw)
    }

    /// Phase 2.2: `fallocate(FALLOC_FL_KEEP_SIZE)` — preallocate blocks
    /// in the byte range `[offset, offset+len)` as uninitialized
    /// extents. The blocks are reserved (count against `i_blocks`) but
    /// reads return zeros until they're written. `i_size` is left
    /// unchanged per KEEP_SIZE semantics.
    ///
    /// v1 limitations:
    /// - Range must be entirely unmapped — partially-overlapping ranges
    ///   return `Error::InvalidArgument`. (Splitting around existing
    ///   extents is a follow-up.)
    /// - Single contiguous physical allocation. If the bitmap can't
    ///   serve `ceil(len / block_size)` contiguous blocks, returns
    ///   `Error::Corrupt("no group has a contiguous free run...")`.
    /// - The new extent goes into the inline root while it has a free
    ///   slot; a full root is promoted to an index node and a deeper tree
    ///   is descended, with the node blocks drawn in the same transaction.
    pub fn apply_fallocate_keep_size(&self, ino: u32, offset: u64, len: u64) -> Result<()> {
        self.refuse_write()?;
        if len == 0 {
            return Ok(());
        }
        let bs = self.sb.block_size() as u64;
        let bs_u32 = self.sb.block_size();
        let first_block = offset / bs;
        let last_block_excl = offset
            .checked_add(len)
            .ok_or(Error::InvalidArgument("fallocate: offset+len overflow"))?
            .div_ceil(bs);
        let need_blocks_u64 = last_block_excl - first_block;
        if need_blocks_u64 > u32::MAX as u64 {
            return Err(Error::InvalidArgument(
                "fallocate: range exceeds u32 block count",
            ));
        }
        let need_blocks = need_blocks_u64 as u32;

        let (inode, mut raw) = self.read_inode_verified(ino)?;
        if !inode.is_file() {
            return Err(Error::InvalidArgument(
                "fallocate: target is not a regular file",
            ));
        }
        if !inode.has_extents() {
            return Err(Error::InvalidArgument(
                "fallocate: legacy (non-extents) inodes not supported",
            ));
        }

        // V1: refuse if any block in range is already mapped — handling
        // the partial-overlap case requires splitting existing extents
        // mid-range, deferred to a follow-up.
        for log in first_block..last_block_excl {
            if crate::extent::map_logical(&inode.block, self.dev.as_ref(), bs_u32, log)?.is_some() {
                return Err(Error::InvalidArgument(
                    "fallocate: range partially mapped (v1 limitation)",
                ));
            }
        }

        // Allocate one contiguous physical run.
        let inode_group = (ino - 1) / self.sb.inodes_per_group;
        let mut bitmap_reader = |block: u64| self.read_block(block);
        let plan = crate::alloc::plan_block_allocation(
            &self.sb,
            &self.allocation_groups(),
            need_blocks,
            inode_group,
            &mut bitmap_reader,
        )?;

        // Insert as an uninitialized extent so reads see zeros without
        // hitting disk. Clamp to u16 — the range check above already
        // bounded need_blocks, but the on-disk extent length is u16.
        if need_blocks > 0x7FFF {
            return Err(Error::InvalidArgument(
                "fallocate: single-extent length > 32K blocks (split needed)",
            ));
        }
        let new_extent = crate::extent::Extent {
            logical_block: first_block as u32,
            length: need_blocks as u16,
            physical_block: plan.first_block,
            uninitialized: true,
        };
        // Apply via BlockBuffer — atomic across bitmap, BGD, SB, inode.
        // The data run is staged first, so the tree nodes a promotion
        // allocates below are drawn around it.
        let mut buf = BlockBuffer::new(self.sb.block_size());
        self.buffer_mark_block_run_used(&mut buf, plan.first_block, need_blocks as u64)?;
        self.buffer_patch_bgd_counters(
            &mut buf,
            plan.bgd.group_idx as usize,
            plan.bgd.free_blocks_delta,
            plan.bgd.free_inodes_delta,
            plan.bgd.used_dirs_delta,
        )?;

        // The inline root holds four extents. A fifth promotes it to an
        // index node, and a root that already is one has to be descended:
        // both go through the deep planner, as `apply_pwrite` does.
        let mut meta_blocks: u64 = 0;
        let new_root = match crate::extent_mut::plan_insert_extent(&inode.block, new_extent) {
            Ok(muts) => muts
                .into_iter()
                .find_map(|m| match m {
                    crate::extent_mut::ExtentMutation::WriteRoot { bytes } => Some(bytes),
                    _ => None,
                })
                .ok_or(Error::Corrupt("fallocate: extent insert wrote no root"))?,
            Err(Error::CorruptExtentTree(msg))
                if msg.contains("LEAF_FULL_NEEDS_PROMOTION")
                    || msg.contains("multi-level tree mutation") =>
            {
                let reader = FsBlockReader { fs: self };
                let deep_plan = {
                    let mut alloc_node = || -> Result<u64> {
                        let p = self.plan_buffered_block_allocation(&buf, 1, inode_group)?;
                        self.buffer_mark_block_run_used(&mut buf, p.first_block, 1)?;
                        self.buffer_patch_bgd_counters(
                            &mut buf,
                            p.bgd.group_idx as usize,
                            p.bgd.free_blocks_delta,
                            0,
                            0,
                        )?;
                        meta_blocks += 1;
                        Ok(p.first_block)
                    };
                    crate::extent_mut::plan_insert_extent_deep(
                        &inode.block,
                        new_extent,
                        bs_u32,
                        &reader,
                        &mut alloc_node,
                    )?
                };
                for (block, mut bytes) in deep_plan.block_writes {
                    if self.csum.enabled {
                        self.csum
                            .patch_extent_tail(ino, inode.generation, &mut bytes);
                    }
                    buf.put(block, bytes);
                }
                deep_plan.new_root
            }
            Err(e) => return Err(e),
        };
        self.buffer_patch_sb_counters(
            &mut buf,
            plan.sb.free_blocks_delta - meta_blocks as i64,
            plan.sb.free_inodes_delta,
        )?;

        // Splice the new extent root into the inode image.
        Self::patch_inode_block_area(&mut raw, &new_root)?;

        // Bump i_blocks (sectors). KEEP_SIZE: i_size unchanged.
        let sectors_per_block = bs / 512;
        let new_i_blocks = inode
            .blocks
            .saturating_add((need_blocks as u64 + meta_blocks) * sectors_per_block);
        Self::patch_inode_size_and_blocks(&mut raw, inode.size, new_i_blocks)?;

        // POSIX: fallocate bumps mtime + ctime.
        let now = self.runtime.now_unix_seconds();
        set_inode_time(&mut raw, InodeTime::Ctime, now);
        set_inode_time(&mut raw, InodeTime::Mtime, now);

        self.finalize_inode_raw(ino, inode.generation, &mut raw)?;
        self.buffer_write_inode(&mut buf, ino, &raw)?;

        self.commit_block_buffer(buf)
    }

    /// Phase 2.3 — `fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)`.
    /// Frees the data blocks wholly inside `[offset, offset+len)`, splitting
    /// straddling extents as needed, and zeroes the part of a block at
    /// either end that the range covers only in part. Reads of the punched range return
    /// zeros (sparse hole) thereafter; `i_size` is unchanged.
    ///
    /// Extent trees of any depth. The surviving extents are laid out
    /// again by `extent_mut::plan_repack_tree` over the tree blocks the
    /// file already holds (#258): four or fewer go in the inode, more get
    /// as many leaf and index levels as they need, and tree blocks left
    /// unused are freed with the data. A punch inside a single extent
    /// leaves a head and a tail where there was one record, so the layout
    /// can be one block short; that block is allocated, and a full volume
    /// fails the punch with the allocator's error rather than for size.
    ///
    /// Limits:
    /// - Indirect-block (ext2/3) inodes return EINVAL — punch is an
    ///   ext4-specific kernel API.
    pub fn apply_fallocate_punch_hole(&self, ino: u32, offset: u64, len: u64) -> Result<()> {
        self.refuse_write()?;
        if len == 0 {
            return Ok(());
        }
        let bs = self.sb.block_size() as u64;
        let bs_u32 = self.sb.block_size();
        let end = offset
            .checked_add(len)
            .ok_or(Error::InvalidArgument("punch_hole: offset+len overflow"))?;
        // ONLY THE BLOCKS WHOLLY INSIDE THE RANGE ARE FREED (#388). A block
        // the range covers in part keeps its other bytes, so it stays mapped
        // and the covered part is zeroed in place, as the kernel does. This
        // rounded outward once, and `punch(100, 100)` zeroed bytes 0..4096.
        let punch_first = offset.div_ceil(bs);
        let punch_last_excl = end / bs;

        let (inode, mut raw) = self.read_inode_verified(ino)?;
        if !inode.is_file() {
            return Err(Error::InvalidArgument("punch_hole: not a regular file"));
        }
        if !inode.has_extents() {
            return Err(Error::InvalidArgument(
                "punch_hole: legacy (non-extents) inodes not supported",
            ));
        }

        let (extents, tree_nodes) =
            crate::extent::collect_all_with_nodes(&inode.block, self.dev.as_ref(), bs_u32)?;
        let mut new_entries: Vec<crate::extent::Extent> = Vec::new();
        let mut freed_blocks: u64 = 0;
        let mut buf = BlockBuffer::new(bs_u32);

        // The partial edge blocks: zeroed in the same transaction. A hole or
        // an uninitialized extent already reads as zeros and is left alone.
        let (head_block, tail_block) = (offset / bs, end / bs);
        let edges = if head_block == tail_block {
            vec![head_block]
        } else {
            vec![head_block, tail_block]
        };
        for lb in edges {
            let lo = offset.max(lb * bs);
            let hi = end.min((lb + 1) * bs);
            if lo >= hi || hi - lo == bs {
                continue; // outside the range, or wholly inside it
            }
            let Some(phys) =
                crate::extent::map_logical(&inode.block, self.dev.as_ref(), bs_u32, lb)?
            else {
                continue;
            };
            let block = buf.get_mut(self, phys)?;
            let (from, to) = ((lo - lb * bs) as usize, (hi - lb * bs) as usize);
            block[from..to].fill(0);
        }

        // A range inside one block, or across two with no whole block
        // between, frees nothing: the tree is left exactly as it is.
        let frees_blocks = punch_first < punch_last_excl;
        for e in extents.iter().filter(|_| frees_blocks) {
            let el = e.logical_block as u64;
            let er = el + e.length as u64;

            if er <= punch_first || el >= punch_last_excl {
                // Fully outside the punch range — keep verbatim.
                new_entries.push(*e);
                continue;
            }
            if el >= punch_first && er <= punch_last_excl {
                // Fully inside punch — free entirely.
                freed_blocks += self.buffer_free_block_run_and_bgd(
                    &mut buf,
                    e.physical_block,
                    e.length as u64,
                )?;
                continue;
            }
            // Partial overlap. Compute the freed sub-range; emit head /
            // tail retains around it.
            let free_lo = el.max(punch_first);
            let free_hi = er.min(punch_last_excl);
            let free_offset_in_e = free_lo - el;
            let free_len = (free_hi - free_lo) as u32;
            let free_phys = e.physical_block + free_offset_in_e;
            freed_blocks +=
                self.buffer_free_block_run_and_bgd(&mut buf, free_phys, free_len as u64)?;

            if el < punch_first {
                new_entries.push(crate::extent::Extent {
                    logical_block: el as u32,
                    length: (punch_first - el) as u16,
                    physical_block: e.physical_block,
                    uninitialized: e.uninitialized,
                });
            }
            if er > punch_last_excl {
                new_entries.push(crate::extent::Extent {
                    logical_block: punch_last_excl as u32,
                    length: (er - punch_last_excl) as u16,
                    physical_block: e.physical_block + (punch_last_excl - el),
                    uninitialized: e.uninitialized,
                });
            }
        }

        // THE TREE IS LAID OUT AGAIN OVER THE BLOCKS IT ALREADY HELD (#258).
        // The survivors are a subset of the entries the tree held, so packing
        // them full needs no more blocks than it has, and the ones left over
        // go back to free space with the data blocks. Four or fewer survivors
        // need no blocks at all and go in the inode, which is all this used
        // to do: every file with more than four surviving extents — that is,
        // every large file, which is what a punch is for — was refused.
        let mut allocated_blocks = 0;
        if frees_blocks {
            let gen = u32::from_le_bytes(inode.block[8..12].try_into().unwrap());
            let repacked = {
                let mut alloc = || self.buffer_allocate_block(&mut buf, ino);
                crate::extent_mut::plan_repack_tree(
                    gen,
                    &new_entries,
                    bs_u32,
                    &tree_nodes,
                    &mut alloc,
                )?
            };
            allocated_blocks = repacked.allocated_blocks.len() as u64;
            for (block, mut bytes) in repacked.block_writes {
                if self.csum.enabled {
                    self.csum
                        .patch_extent_tail(ino, inode.generation, &mut bytes);
                }
                buf.put(block, bytes);
            }
            for &node in &tree_nodes {
                if repacked.used_nodes.contains(&node) {
                    continue;
                }
                freed_blocks += self.buffer_free_block_run_and_bgd(&mut buf, node, 1)?;
            }
            Self::patch_inode_block_area(&mut raw, &repacked.new_root)?;
        }

        // i_blocks decreases; i_size unchanged (KEEP_SIZE semantics
        // built in — punch always preserves size).
        let sectors_per_block = bs / 512;
        let new_i_blocks = inode
            .blocks
            .saturating_sub(freed_blocks * sectors_per_block)
            + allocated_blocks * sectors_per_block;
        Self::patch_inode_size_and_blocks(&mut raw, inode.size, new_i_blocks)?;
        let now = self.runtime.now_unix_seconds();
        set_inode_time(&mut raw, InodeTime::Ctime, now);
        set_inode_time(&mut raw, InodeTime::Mtime, now);
        self.finalize_inode_raw(ino, inode.generation, &mut raw)?;
        self.buffer_write_inode(&mut buf, ino, &raw)?;

        if freed_blocks > 0 {
            self.buffer_patch_sb_counters(&mut buf, freed_blocks as i64, 0)?;
        }

        self.commit_block_buffer(buf)
    }

    /// Phase 2.4 — `fallocate(FALLOC_FL_ZERO_RANGE)`. Logically zero the
    /// byte range `[offset, offset+len)`. Implemented as punch-hole, which
    /// zeroes the partial edge blocks in place, + KEEP_SIZE preallocate of
    /// the whole blocks between them, so reads return zeros
    /// (uninitialized-extent semantics) and future writes don't need an
    /// allocation.
    ///
    /// Two separate transactions today (punch then alloc); a future
    /// optimization could fold them into one.
    pub fn apply_fallocate_zero_range(&self, ino: u32, offset: u64, len: u64) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        self.apply_fallocate_punch_hole(ino, offset, len)?;
        // Only the whole blocks the punch freed are preallocated again: the
        // partial ones at the edges stay mapped, their range zeroed (#388).
        let bs = self.sb.block_size() as u64;
        let end = offset
            .checked_add(len)
            .ok_or(Error::InvalidArgument("zero_range: offset+len overflow"))?;
        let (first, last) = (offset.div_ceil(bs) * bs, end / bs * bs);
        if first >= last {
            return Ok(());
        }
        self.apply_fallocate_keep_size(ino, first, last - first)
    }

    /// Change the permission bits on `path`. Only the low 12 bits of `mode`
    /// (`S_ISUID|S_ISGID|S_ISVTX` plus rwx/rwx/rwx) are applied; the file-type
    /// bits (`S_IFMT`) are preserved from the existing inode.
    ///
    /// Updates `i_ctime = now` and recomputes the inode checksum on csum-
    /// enabled mounts. Returns `Error::NotFound` if the path doesn't resolve,
    /// `Error::ReadOnly` on a RO mount.
    pub fn apply_chmod(&self, path: &str, mode: u16) -> Result<()> {
        self.apply_chmod_bytes(path.as_bytes(), mode)
    }

    /// [`apply_chmod`](Self::apply_chmod) of a path given as bytes, never decoded.
    pub(crate) fn apply_chmod_bytes(&self, path: &[u8], mode: u16) -> Result<()> {
        self.refuse_write()?;
        let ino = self.resolve(path)?;
        self.apply_chmod_ino(ino, mode)
    }

    /// [`apply_chmod`](Self::apply_chmod) on the inode `r` names.
    pub fn apply_chmod_ino(&self, r: impl Into<InodeRef>, mode: u16) -> Result<()> {
        self.refuse_write()?;
        let r = r.into();
        let ino = r.ino;
        let (inode, mut raw) = self.live_inode(r)?;

        // Preserve file-type bits (high 4 bits of i_mode); only the low 12
        // permission/suid/sgid/sticky bits are user-settable.
        let file_type_bits = inode.mode & crate::inode::S_IFMT;
        let new_mode = file_type_bits | (mode & 0x0FFF);
        raw[0x00..0x02].copy_from_slice(&new_mode.to_le_bytes());

        // POSIX: chmod bumps ctime (not mtime).
        let now = self.runtime.now_unix_seconds();
        set_inode_time(&mut raw, InodeTime::Ctime, now);

        self.finalize_inode_raw(ino, inode.generation, &mut raw)?;
        self.commit_inode_write(ino, &raw)
    }

    /// Write a single mutated inode back, routing through the journal
    /// writer when one is available so the change is crash-safe. Falls
    /// back to a direct write + flush on unjournaled mounts.
    ///
    /// Used by every operation whose only mutation is one inode block:
    /// chmod, chown, utimens, and the in-place xattr ops once they're
    /// migrated to the journaled path.
    fn commit_inode_write(&self, ino: u32, new_inode_raw: &[u8]) -> Result<()> {
        let mut buf = BlockBuffer::new(self.sb.block_size());
        self.buffer_write_inode(&mut buf, ino, new_inode_raw)?;
        self.commit_block_buffer(buf)
    }

    // ----------------------------------------------------------------------
    // BlockBuffer helpers (Phase 5.2 multi-block transactions)
    // ----------------------------------------------------------------------
    //
    // These mirror the disk-touching helpers (free_block_run_and_bgd,
    // patch_bgd_counters, patch_sb_counters, write_inode_raw) but operate
    // on an in-memory BlockBuffer instead. A multi-block op accumulates
    // its mutations into one buffer and commits the whole thing atomically
    // — either through the journal writer (when present) or via a flush-
    // gated direct-write fallback.

    /// Splice a freshly-built inode into the inode-table block buffer.
    pub(crate) fn buffer_write_inode(
        &self,
        buf: &mut BlockBuffer,
        ino: u32,
        inode_raw: &[u8],
    ) -> Result<()> {
        let (block, offset) = bgd::locate_inode(&self.sb, &self.groups, ino)?;
        let it_buf = buf.get_mut(self, block)?;
        let off = offset as usize;
        it_buf[off..off + inode_raw.len()].copy_from_slice(inode_raw);
        Ok(())
    }

    /// Every block an extent-mapped inode holds, as runs: its data extents,
    /// and the index and leaf blocks below its inline root.
    ///
    /// Read from the tree, not from `i_size`. A file of size zero can hold
    /// blocks (a `KEEP_SIZE` preallocation), and blocks can lie past the
    /// size of any file. A caller that frees a whole file frees these.
    ///
    /// Every index and leaf block must pass its checksum first, or this
    /// is [`Error::BadChecksum`] and nothing is returned to free: a node
    /// that does not verify may name blocks that belong to another file,
    /// and the kernel refuses the removal the same way.
    pub(crate) fn extent_tree_runs(&self, ino: u32, inode: &Inode) -> Result<Vec<(u64, u64)>> {
        let (extents, nodes) = crate::extent::collect_all_with_nodes(
            &inode.block,
            self.dev.as_ref(),
            self.sb.block_size(),
        )?;
        for &node in &nodes {
            let bytes = self.read_block(node)?;
            if !self.csum.verify_extent_tail(ino, inode.generation, &bytes) {
                return Err(Error::BadChecksum {
                    what: "extent block",
                });
            }
        }
        Ok(extents
            .iter()
            .map(|e| (e.physical_block, e.length as u64))
            .chain(nodes.into_iter().map(|n| (n, 1)))
            .collect())
    }

    /// Free every run `extent_tree_runs` lists, returning the block count.
    pub(crate) fn buffer_free_runs(
        &self,
        buf: &mut BlockBuffer,
        runs: &[(u64, u64)],
    ) -> Result<u64> {
        let mut freed = 0;
        for &(start, len) in runs {
            freed += self.buffer_free_block_run_and_bgd(buf, start, len)?;
        }
        Ok(freed)
    }

    /// Buffer-side equivalent of `free_block_run_and_bgd`: clears the
    /// bitmap bits AND patches the BGD counters in the buffer. Returns
    /// `len` so callers can accumulate a running freed-block total to
    /// feed to `buffer_patch_sb_counters`.
    ///
    /// A run may cross a group boundary -- the kernel merges extents on
    /// adjacency alone, and so does `extent_mut::are_contiguous` -- so it
    /// is freed one group at a time (#118). It used to take the group of
    /// `start` alone: the bits past that group were dropped, and its
    /// descriptor was credited the whole length, so the next group's
    /// blocks stayed allocated for ever while the first group over-counted.
    pub(crate) fn buffer_free_block_run_and_bgd(
        &self,
        buf: &mut BlockBuffer,
        start: u64,
        len: u64,
    ) -> Result<u64> {
        for (gi, bit_start, chunk) in self.group_chunks(start, len)? {
            let bitmap_block = self.groups[gi].block_bitmap;
            {
                let bm = buf.get_mut(self, bitmap_block)?;
                for i in 0..chunk {
                    let bit = bit_start + i;
                    let byte = (bit / 8) as usize;
                    let mask = 1u8 << (bit % 8);
                    if byte < bm.len() {
                        bm[byte] &= !mask;
                    }
                }
            }
            self.buffer_refresh_bitmap_csum(buf, gi, false)?;
            self.buffer_patch_bgd_counters(buf, gi, chunk as i32, 0, 0)?;
        }
        Ok(len)
    }

    /// `[start, start + len)` split at block-group boundaries, as
    /// `(group, bit within the group, length)` for each group it touches.
    ///
    /// Every group is validated, not only the first: a run whose tail
    /// leaves the last group is refused rather than truncated. A caller
    /// working in a `BlockBuffer` drops it uncommitted on that error.
    fn group_chunks(&self, start: u64, len: u64) -> Result<Vec<(usize, u64, u64)>> {
        let bpg = self.sb.blocks_per_group as u64;
        let first_data = self.sb.first_data_block as u64;
        let end = start.checked_add(len).ok_or(Error::InvalidBlock(start))?;
        if start < first_data || bpg == 0 {
            return Err(Error::InvalidBlock(start));
        }
        // The last group is usually short, and its nominal span past
        // `blocks_count` is padding: bits that stand for no block. A run
        // reaching into it passed the group-index check below, and freeing
        // it cleared padding bits and credited blocks that do not exist
        // (Greptile on #188).
        if end > self.sb.blocks_count {
            return Err(Error::InvalidBlock(self.sb.blocks_count));
        }
        let mut chunks = Vec::new();
        let mut at = start;
        while at < end {
            let gi = ((at - first_data) / bpg) as usize;
            if gi >= self.groups.len() {
                return Err(Error::InvalidBlock(at));
            }
            let group_start = first_data + gi as u64 * bpg;
            let chunk = end.min(group_start + bpg) - at;
            chunks.push((gi, at - group_start, chunk));
            at += chunk;
        }
        Ok(chunks)
    }

    /// Rewrite the checksum of the group descriptor at `block[off..]` (group
    /// `gi`) under whichever scheme the volume uses: crc32c for
    /// `METADATA_CSUM`, crc16 for `GDT_CSUM`, nothing for neither.
    fn restamp_group_desc_csum(&self, block: &mut [u8], off: usize, gi: usize) {
        let end = off + self.sb.desc_size as usize;
        if let Some(c) =
            crate::checksum::group_desc_csum(&self.sb, &self.csum, gi as u32, &block[off..end])
        {
            block[off + 0x1E..off + 0x20].copy_from_slice(&c.to_le_bytes());
        }
    }

    /// Buffer-side equivalent of `mark_block_run_used`: sets the bitmap
    /// bits for `[start, start+len)` in the buffer's bitmap block.
    /// If group `gi`'s BGD has the given uninit flag set, clear it in `buf`
    /// and return `true` (the caller must then zero the bitmap block
    /// itself — the flag being set is precisely the license callers had to
    /// leave that block's on-disk content unspecified). Returns `false`,
    /// no-op, if the flag was already clear.
    fn clear_bgd_uninit_flag_if_set(
        &self,
        buf: &mut BlockBuffer,
        gi: usize,
        which: BgdUninitFlag,
    ) -> Result<bool> {
        let flag = match which {
            BgdUninitFlag::Inode => bgd::BgdFlags::INODE_UNINIT.bits(),
            BgdUninitFlag::Block => bgd::BgdFlags::BLOCK_UNINIT.bits(),
        };

        // Where the descriptor lives, META_BG or not (#73).
        let (bgt_block, off) = self.sb.descriptor_location(gi as u64);

        let block = buf.get_mut(self, bgt_block)?;
        let flags_off = off + 0x12;
        let flags = u16::from_le_bytes(block[flags_off..flags_off + 2].try_into().unwrap());
        if flags & flag == 0 {
            return Ok(false);
        }
        let new_flags = flags & !flag;
        block[flags_off..flags_off + 2].copy_from_slice(&new_flags.to_le_bytes());
        // Record it against the mount-time snapshot too, or the very next
        // allocation plans as though the group were still untouched — but
        // record it on the *buffer*, so it becomes visible only when the
        // buffer commits.
        //
        // Publishing it here instead would survive a failed commit: the
        // operation returns an error, the mount carries on, and the next
        // allocation is told the group's bitmap is initialised while the
        // bytes on disk are still whatever the uninit flag licensed
        // leaving there.
        buf.uninit_cleared
            .entry(gi)
            .and_modify(|f| *f &= !flag)
            .or_insert(new_flags);
        Ok(true)
    }

    /// The group descriptors the allocators must plan against: the mount-time
    /// snapshot, with any uninit flag this mount has since cleared taken back
    /// out. Borrows the snapshot untouched in the overwhelmingly common case
    /// where nothing has been cleared yet.
    ///
    /// Otherwise the overridden copy is built once and shared until the next
    /// clear is published (#333): this is called on every allocation,
    /// several times inside the write retry loop, and cloning every
    /// descriptor of a large volume each time cost more than the plan.
    fn allocation_groups(&self) -> AllocationGroups<'_> {
        let mut state = self.uninit_cleared.lock().unwrap();
        if state.cleared.is_empty() {
            return AllocationGroups::Snapshot(&self.groups);
        }
        if state.overridden.is_none() {
            let mut groups = self.groups.clone();
            for (&gi, &flags) in state.cleared.iter() {
                groups[gi].flags = flags;
            }
            state.overridden = Some(groups.into());
        }
        AllocationGroups::Overridden(state.overridden.clone().unwrap())
    }

    /// Plan a block allocation inside an open transaction: bitmaps come from
    /// the buffer when it has staged them, and so do uninit flags.
    ///
    /// The second half is the one that matters. Staging an allocation into a
    /// BLOCK_UNINIT group clears the flag on the buffer only, so it becomes
    /// visible to the mount when the buffer commits. A plan that still sees
    /// the flag synthesises the bitmap from the group's metadata and never
    /// reads the staged one, so it offers the run just handed out again.
    fn plan_buffered_block_allocation(
        &self,
        buf: &BlockBuffer,
        count: u32,
        hint: u32,
    ) -> Result<crate::alloc::BlockAllocationPlan> {
        self.plan_buffered_block_allocation_excluding(buf, count, hint, &[])
    }

    /// [`Self::plan_buffered_block_allocation`], with `reserved` blocks --
    /// spoken for but not yet staged as used -- treated as used.
    fn plan_buffered_block_allocation_excluding(
        &self,
        buf: &BlockBuffer,
        count: u32,
        hint: u32,
        reserved: &[u64],
    ) -> Result<crate::alloc::BlockAllocationPlan> {
        let base = self.allocation_groups();
        let mut groups = Cow::Borrowed(&*base);
        for (&gi, &flags) in &buf.uninit_cleared {
            if let Some(g) = groups.to_mut().get_mut(gi) {
                g.flags = flags;
            }
        }
        crate::alloc::plan_block_allocation_excluding(
            &self.sb,
            &groups,
            count,
            hint,
            reserved,
            |block| match buf.dirty.get(&block) {
                Some(bytes) => Ok(bytes.clone()),
                None => self.read_block(block),
            },
        )
    }

    pub(crate) fn buffer_mark_block_run_used(
        &self,
        buf: &mut BlockBuffer,
        start: u64,
        len: u64,
    ) -> Result<()> {
        let bpg = self.sb.blocks_per_group as u64;
        let first_data = self.sb.first_data_block as u64;
        let gi = ((start - first_data) / bpg) as usize;
        if gi >= self.groups.len() {
            return Err(Error::InvalidBlock(start));
        }
        let group_start = first_data + gi as u64 * bpg;
        let bit_start = (start - group_start) as u32;

        // Same staleness problem as `buffer_mark_inode_used`, for the block
        // bitmap this time: BLOCK_UNINIT is every reader's license to skip
        // the on-disk bitmap and treat the group as empty, so the *next*
        // mount kept proposing the same "first free" block for every new
        // allocation into this group — including a file's own data block
        // landing on top of a directory's just-created data block in the
        // same group. Reproduced by hand: the second file written into a
        // freshly-created directory corrupted the directory's own data
        // block ("corrupt directory entry: bad rec_len during add") because
        // its content block silently reused the directory's block number.
        //
        // Unlike an uninit inode bitmap, "all blocks free" isn't quite
        // right here: a group still owns whatever fixed overhead physically
        // lives inside it, and zeroing the bitmap without putting that back
        // hands the group's own metadata out as free space. Two kinds of
        // overhead can be there — the RO_COMPAT_SPARSE_SUPER superblock +
        // GDT backup (groups 0, 1, and powers of 3/5/7), and the group's own
        // block bitmap, inode bitmap and inode table.
        //
        // With flex_bg those last three usually sit in the cohort's head
        // group, and a group is only left BLOCK_UNINIT when mkfs had no real
        // bitmap/table data to write for it — so on a flex_bg volume they are
        // reliably elsewhere. That is an assumption about the formatter,
        // though, not something the on-disk format guarantees: without
        // flex_bg every group holds its own. So rather than assume, ask where
        // the descriptor actually points and reserve whatever lands inside
        // this group.
        let was_uninit = self.clear_bgd_uninit_flag_if_set(buf, gi, BgdUninitFlag::Block)?;
        let reserved_runs = if was_uninit {
            crate::alloc::group_owned_metadata_runs(&self.sb, &self.groups, gi)
        } else {
            Vec::new()
        };
        let bitmap_block = self.groups[gi].block_bitmap;
        let bm = buf.get_mut(self, bitmap_block)?;
        if was_uninit {
            bm.iter_mut().for_each(|byte| *byte = 0);
            for (first_bit, count) in reserved_runs {
                for bit in first_bit..(first_bit + count).min(bpg) {
                    let byte = (bit / 8) as usize;
                    let mask = 1u8 << (bit % 8);
                    if byte < bm.len() {
                        bm[byte] |= mask;
                    }
                }
            }
            // Bits past the group's last block are set, as the kernel's
            // `ext4_mark_bitmap_end` sets them: e2fsck reports "Padding at
            // end of block bitmap is not set" otherwise. Every group whose
            // blocks_per_group is under the bitmap block's 8 * block_size
            // bits has some, and a short last group more.
            let in_group = u64::from(crate::alloc::blocks_in_group(&self.sb, gi as u32));
            for bit in in_group..(bm.len() as u64 * 8) {
                bm[(bit / 8) as usize] |= 1u8 << (bit % 8);
            }
        }
        for i in 0..len {
            let bit = bit_start as u64 + i;
            let byte = (bit / 8) as usize;
            let mask = 1u8 << (bit % 8);
            if byte < bm.len() {
                bm[byte] |= mask;
            }
        }
        self.buffer_refresh_bitmap_csum(buf, gi, false)?;
        Ok(())
    }

    /// Recompute a group's bitmap checksum (inode or block) after its bitmap
    /// block changed, then refresh the BGD checksum. metadata_csum stores the
    /// bitmap crc split lo + hi in the descriptor (inode: 0x1A/0x3A, block:
    /// 0x18/0x38); a stale value makes e2fsck and the kernel report "bitmap
    /// does not match checksum". No-op when checksums are disabled.
    pub(crate) fn buffer_refresh_bitmap_csum(
        &self,
        buf: &mut BlockBuffer,
        gi: usize,
        inode_bitmap: bool,
    ) -> Result<()> {
        if !self.csum.enabled {
            return Ok(());
        }
        let (bitmap_block, coverage, lo_off, hi_off) = if inode_bitmap {
            (
                self.groups[gi].inode_bitmap,
                (self.sb.inodes_per_group as usize).div_ceil(8),
                0x1A,
                0x3A,
            )
        } else {
            (
                self.groups[gi].block_bitmap,
                (self.sb.blocks_per_group as usize).div_ceil(8),
                0x18,
                0x38,
            )
        };
        let csum = {
            let bm = buf.get_mut(self, bitmap_block)?;
            let end = coverage.min(bm.len());
            crate::checksum::linux_crc32c(self.csum.seed, &bm[..end])
        };

        let desc_size = self.sb.desc_size as u64;
        // Where the descriptor lives, META_BG or not (#73).
        let (bgt_block, off) = self.sb.descriptor_location(gi as u64);
        let has_hi = desc_size >= 0x40;
        let block = buf.get_mut(self, bgt_block)?;
        block[off + lo_off..off + lo_off + 2]
            .copy_from_slice(&((csum & 0xFFFF) as u16).to_le_bytes());
        if has_hi {
            block[off + hi_off..off + hi_off + 2]
                .copy_from_slice(&(((csum >> 16) & 0xFFFF) as u16).to_le_bytes());
        }
        // Refresh the BGD checksum (0x1E) so the descriptor stays consistent.
        self.restamp_group_desc_csum(block, off, gi);
        Ok(())
    }

    /// Buffer-side equivalent of `free_inode_slot`: clears the inode
    /// bitmap bit AND patches the BGD's `bg_free_inodes_count` (+1) in
    /// the buffer. Matches the kernel's pairing — the SB
    /// `s_free_inodes_count` is the caller's responsibility (one bump
    /// per high-level op, via `buffer_patch_sb_counters`).
    pub(crate) fn buffer_free_inode_slot(&self, buf: &mut BlockBuffer, ino: u32) -> Result<()> {
        let ipg = self.sb.inodes_per_group;
        let gi = ((ino - 1) / ipg) as usize;
        if gi >= self.groups.len() {
            return Err(Error::InvalidInode(ino));
        }
        let bit = ((ino - 1) % ipg) as u64;
        let bitmap_block = self.groups[gi].inode_bitmap;
        {
            let bm = buf.get_mut(self, bitmap_block)?;
            let byte = (bit / 8) as usize;
            let mask = 1u8 << (bit % 8);
            if byte < bm.len() {
                bm[byte] &= !mask;
            }
        }
        self.buffer_refresh_bitmap_csum(buf, gi, true)?;
        self.buffer_patch_bgd_counters(buf, gi, 0, 1, 0)
    }

    /// Buffer-side equivalent of `mark_inode_used`: sets the inode
    /// bitmap bit. BGD/SB counter patches are the caller's
    /// responsibility (different ops want different deltas — e.g.
    /// mkdir bumps `used_dirs_count`).
    pub(crate) fn buffer_mark_inode_used(&self, buf: &mut BlockBuffer, ino: u32) -> Result<()> {
        let ipg = self.sb.inodes_per_group;
        let gi = ((ino - 1) / ipg) as usize;
        if gi >= self.groups.len() {
            return Err(Error::InvalidInode(ino));
        }
        let bit = ((ino - 1) % ipg) as u64;
        let bitmap_block = self.groups[gi].inode_bitmap;

        // If this group's inode bitmap is still INODE_UNINIT, every reader
        // (including a future mount of this same filesystem) is required to
        // ignore whatever bytes are actually on disk there and assume the
        // whole group is free — that's the entire point of the flag, and
        // it's why uninit groups' bitmap blocks are allowed to contain
        // stale/unspecified garbage from mkfs. The moment we allocate a
        // real inode out of such a group, that assumption becomes false, so
        // we must (a) zero the block ourselves before setting our bit —
        // group index > 0 has zero pre-reserved inodes, so "everything but
        // our bit is free" is exactly correct here — and (b) clear the
        // flag. Skipping either step means the *next* mount still treats
        // the group as empty and hands out the same inode number again,
        // silently overwriting whatever was just written here. Found by
        // hand: creating a file/directory whose parent lands in a
        // previously-untouched group corrupted the parent on the very next
        // allocation, every time, until this was fixed.
        let was_uninit = self.clear_bgd_uninit_flag_if_set(buf, gi, BgdUninitFlag::Inode)?;
        let bm = buf.get_mut(self, bitmap_block)?;
        if was_uninit {
            bm.iter_mut().for_each(|byte| *byte = 0);
            // e2fsck convention: bits beyond `inodes_per_group`, up to the
            // end of the bitmap block, represent no real inode and must
            // read as 1 ("in use"), not 0 ("free") — that's what "padding
            // at end of inode bitmap is not set" flags otherwise. Harmless
            // on its own (no inode ever maps there), but worth getting
            // right since we're already the one deciding this block's
            // entire content for the first time.
            let bits_per_block = (bm.len() as u64) * 8;
            for pad_bit in (ipg as u64)..bits_per_block {
                let byte = (pad_bit / 8) as usize;
                let mask = 1u8 << (pad_bit % 8);
                bm[byte] |= mask;
            }
        }
        let byte = (bit / 8) as usize;
        let mask = 1u8 << (bit % 8);
        if byte < bm.len() {
            bm[byte] |= mask;
        }
        self.buffer_refresh_bitmap_csum(buf, gi, true)?;

        // Maintain bg_itable_unused: this inode is now in use, so the count of
        // never-used inodes at the END of the group's table can be no larger
        // than the inodes after this one. A stale value makes e2fsck and the
        // kernel treat freshly-allocated inodes as unused ("references inode
        // found in unused inodes area" / "invalid unused inodes count"). lo at
        // 0x1C, hi at 0x32 (desc_size >= 64). The BGD checksum is recomputed so
        // the change stands alone; the following counter patch recomputes it
        // again harmlessly.
        let floor = ipg.saturating_sub(bit as u32 + 1);
        let desc_size = self.sb.desc_size as u64;
        // Where the descriptor lives, META_BG or not (#73).
        let (bgt_block, off) = self.sb.descriptor_location(gi as u64);
        let has_hi = desc_size >= 0x40;
        let block = buf.get_mut(self, bgt_block)?;
        let cur_lo = u16::from_le_bytes(block[off + 0x1C..off + 0x1E].try_into().unwrap()) as u32;
        let cur_hi = if has_hi {
            u16::from_le_bytes(block[off + 0x32..off + 0x34].try_into().unwrap()) as u32
        } else {
            0
        };
        let cur = (cur_hi << 16) | cur_lo;
        if floor < cur {
            block[off + 0x1C..off + 0x1E].copy_from_slice(&((floor & 0xFFFF) as u16).to_le_bytes());
            if has_hi {
                block[off + 0x32..off + 0x34]
                    .copy_from_slice(&(((floor >> 16) & 0xFFFF) as u16).to_le_bytes());
            }
            self.restamp_group_desc_csum(block, off, gi);
        }
        Ok(())
    }

    /// Buffer-side BGD counter patch. Mirrors `patch_bgd_counters` byte
    /// for byte; only the I/O target differs (the BGD block is read from
    /// the buffer if already touched, else from disk).
    pub(crate) fn buffer_patch_bgd_counters(
        &self,
        buf: &mut BlockBuffer,
        gi: usize,
        free_blocks_delta: i32,
        free_inodes_delta: i32,
        used_dirs_delta: i32,
    ) -> Result<()> {
        // Where the descriptor lives, META_BG or not (#73).
        let (bgt_block, off_in_block) = self.sb.descriptor_location(gi as u64);

        let block = buf.get_mut(self, bgt_block)?;
        patch_bgd_counter_fields(
            block,
            off_in_block,
            self.sb.desc_size,
            free_blocks_delta,
            free_inodes_delta,
            used_dirs_delta,
        );

        self.restamp_group_desc_csum(&mut block[..], off_in_block, gi);
        Ok(())
    }

    /// Buffer-side SB counter patch. The SB lives at byte offset 1024
    /// inside the device; for 4 KiB blocks that's offset 1024 within fs
    /// block 0, for 1 KiB blocks the SB IS fs block 1. We patch the
    /// 1024-byte SB region in-place inside the relevant whole block, so
    /// the journal can transport it as a normal full-block write.
    pub(crate) fn buffer_patch_sb_counters(
        &self,
        buf: &mut BlockBuffer,
        free_blocks_delta: i64,
        free_inodes_delta: i32,
    ) -> Result<()> {
        let bs = self.sb.block_size() as u64;
        let sb_offset = crate::superblock::SUPERBLOCK_OFFSET; // 1024
        let sb_block = sb_offset / bs;
        let off_in_block = (sb_offset % bs) as usize;

        let block = buf.get_mut(self, sb_block)?;
        let sb = &mut block[off_in_block..off_in_block + 1024];

        // s_free_inodes_count at 0x10..0x14 (u32 le)
        let fi = u32::from_le_bytes(sb[0x10..0x14].try_into().unwrap()) as i64;
        let fi_new = (fi + free_inodes_delta as i64).max(0) as u32;
        sb[0x10..0x14].copy_from_slice(&fi_new.to_le_bytes());

        // s_free_blocks_count split lo (0x0C..0x10, u32) + hi (0x158..0x15C, u32)
        let lo = u32::from_le_bytes(sb[0x0C..0x10].try_into().unwrap()) as u64;
        let hi = u32::from_le_bytes(sb[0x158..0x15C].try_into().unwrap()) as u64;
        let cur = ((hi << 32) | lo) as i64;
        let new = (cur + free_blocks_delta).max(0) as u64;
        sb[0x0C..0x10].copy_from_slice(&(new as u32).to_le_bytes());
        sb[0x158..0x15C].copy_from_slice(&((new >> 32) as u32).to_le_bytes());

        if self.csum.enabled {
            let csum = crate::checksum::linux_crc32c(!0, &sb[..0x3FC]);
            sb[0x3FC..0x400].copy_from_slice(&csum.to_le_bytes());
        }
        Ok(())
    }

    /// Buffer-side OR of `bits` into the SB's `s_feature_compat` (0x5C).
    pub(crate) fn buffer_patch_sb_compat(&self, buf: &mut BlockBuffer, bits: u32) -> Result<()> {
        let bs = self.sb.block_size() as u64;
        let sb_offset = crate::superblock::SUPERBLOCK_OFFSET;
        let block = buf.get_mut(self, sb_offset / bs)?;
        let off = (sb_offset % bs) as usize;
        let sb = &mut block[off..off + 1024];
        let compat = u32::from_le_bytes(sb[0x5C..0x60].try_into().unwrap());
        sb[0x5C..0x60].copy_from_slice(&(compat | bits).to_le_bytes());
        if self.csum.enabled {
            let csum = crate::checksum::linux_crc32c(!0, &sb[..0x3FC]);
            sb[0x3FC..0x400].copy_from_slice(&csum.to_le_bytes());
        }
        Ok(())
    }

    /// Buffer-side patch of the SB's `s_last_orphan` field at byte
    /// 0xE8. Used by orphan recovery (Phase 6.2) to clear / advance the
    /// chain head atomically with the inode/block frees.
    pub(crate) fn buffer_patch_sb_last_orphan(
        &self,
        buf: &mut BlockBuffer,
        value: u32,
    ) -> Result<()> {
        let bs = self.sb.block_size() as u64;
        let sb_offset = crate::superblock::SUPERBLOCK_OFFSET;
        let sb_block = sb_offset / bs;
        let off_in_block = (sb_offset % bs) as usize;
        let block = buf.get_mut(self, sb_block)?;
        let sb = &mut block[off_in_block..off_in_block + 1024];
        sb[0xE8..0xEC].copy_from_slice(&value.to_le_bytes());
        if self.csum.enabled {
            let csum = crate::checksum::linux_crc32c(!0, &sb[..0x3FC]);
            sb[0x3FC..0x400].copy_from_slice(&csum.to_le_bytes());
        }
        Ok(())
    }

    /// Buffer-side equivalent of `remove_dir_entry`: scans `parent`'s
    /// dir blocks, removes the named entry, recomputes the tail csum,
    /// stages the modified block in `buf`. Returns `Error::NotFound`
    /// when the name isn't present.
    pub(crate) fn buffer_remove_dir_entry(
        &self,
        buf: &mut BlockBuffer,
        parent_ino: u32,
        parent_inode: &Inode,
        name: &[u8],
    ) -> Result<()> {
        let bs = self.sb.block_size();
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
        let n_blocks = parent_inode.size.div_ceil(bs as u64);
        for logical in 0..n_blocks {
            let Some(phys) = self.map_inode_logical(parent_inode, logical)? else {
                continue;
            };
            let block = buf.get_mut(self, phys)?;
            // An index block holds no entries to remove, and read as one it
            // ends in a tail-shaped dt_reserved and a `..` spanning the rest
            // (#233).
            if Self::is_htree_index_block(parent_inode, logical, block) {
                continue;
            }
            let reserved_tail = if self.csum.enabled && crate::dir::has_csum_tail(block) {
                12
            } else {
                0
            };
            if crate::dir::remove_entry_from_block(block, name, has_ft, reserved_tail)? {
                if self.csum.enabled && reserved_tail == 12 {
                    self.csum
                        .patch_dir_entry_tail(parent_ino, parent_inode.generation, block);
                }
                return Ok(());
            }
        }
        Err(Error::NotFound)
    }

    /// Buffer-side equivalent of `update_dotdot`: rewrites the `..`
    /// entry in `dir_inode`'s first data block (in-buffer) to point at
    /// `new_parent_ino`, recomputes the tail csum.
    pub(crate) fn buffer_update_dotdot(
        &self,
        buf: &mut BlockBuffer,
        dir_ino: u32,
        dir_inode: &Inode,
        new_parent_ino: u32,
    ) -> Result<()> {
        let phys = self
            .map_inode_logical(dir_inode, 0)?
            .ok_or(Error::Corrupt("buffer_update_dotdot: dir block 0 missing"))?;
        let block = buf.get_mut(self, phys)?;
        if block.len() < 24 {
            return Err(Error::Corrupt("buffer_update_dotdot: dir block too small"));
        }
        // Checked before it is changed, linear or indexed: a fresh checksum
        // over a block that was already corrupt would hide the corruption.
        // Nothing earlier in a rename reads this block — the lookups read
        // the parents — so this is the only place it can be refused (#322).
        self.refuse_unverified_dir_block(dir_ino, dir_inode, 0, block)?;
        block[12..16].copy_from_slice(&new_parent_ino.to_le_bytes());
        if dir_inode.flags & crate::inode::InodeFlags::INDEX.bits() != 0 {
            // A dx_root's checksum is its dx_tail's, over a different range
            // by a different rule. It can end in bytes that look like a
            // dirent tail, and writing one there corrupted the index.
            self.csum
                .patch_dx_tail(dir_ino, dir_inode.generation, block, 32);
        } else if self.csum.enabled && crate::dir::has_csum_tail(block) {
            self.csum
                .patch_dir_entry_tail(dir_ino, dir_inode.generation, block);
        }
        Ok(())
    }

    /// Buffer-side equivalent of `add_dir_entry` for the IN-PLACE case
    /// only (an existing parent block has room for the new entry). The
    /// dir block is read into the buffer (or reused if already touched),
    /// `add_entry_to_block` rewrites it, csum patched, returns Ok(()).
    ///
    /// Returns `Error::OutOfBounds` when no existing parent block has
    /// room — caller should then fall through to
    /// `buffer_extend_dir_and_add_entry` to grow the directory by one
    /// block (which has its own scope limits).
    pub(crate) fn buffer_add_dir_entry_inplace(
        &self,
        buf: &mut BlockBuffer,
        parent_ino: u32,
        parent_inode: &Inode,
        name: &[u8],
        target_ino: u32,
        file_type: crate::dir::DirEntryType,
    ) -> Result<()> {
        let bs = self.sb.block_size();
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
        if parent_inode.flags & crate::inode::InodeFlags::INDEX.bits() != 0 {
            return self.buffer_add_dir_entry_indexed(
                buf,
                parent_ino,
                parent_inode,
                name,
                target_ino,
                file_type,
                has_ft,
            );
        }
        let n_blocks = parent_inode.size.div_ceil(bs as u64);
        for logical in 0..n_blocks {
            let Some(phys) = self.map_inode_logical(parent_inode, logical)? else {
                continue;
            };
            let block = buf.get_mut(self, phys)?;
            let reserved_tail = if self.csum.enabled && crate::dir::has_csum_tail(block) {
                12
            } else {
                0
            };
            match crate::dir::add_entry_to_block(
                block,
                target_ino,
                name,
                file_type,
                has_ft,
                reserved_tail,
            ) {
                Ok(()) => {
                    if self.csum.enabled && reserved_tail == 12 {
                        self.csum
                            .patch_dir_entry_tail(parent_ino, parent_inode.generation, block);
                    }
                    return Ok(());
                }
                Err(Error::OutOfBounds) => continue,
                Err(e) => return Err(e),
            }
        }
        // No existing block has room — caller must extend the directory.
        Err(Error::OutOfBounds)
    }

    /// [`Self::buffer_add_dir_entry_inplace`] for a directory with
    /// `EXT4_INDEX_FL`.
    ///
    /// Block 0 of such a directory is the `dx_root`, and interior nodes are
    /// blocks too. None of them is a place for an entry: the root's fake
    /// `..` spans the rest of its block, so treating it as a linear block
    /// wrote the new entry over `dx_root_info` and zeroed the `dx_entry`
    /// array (#97). The entry goes where the index says a lookup will look,
    /// the leaf covering its hash, or nowhere.
    ///
    /// `Error::OutOfBounds` when that leaf is full, or when the names here
    /// are hashed some way this crate does not (casefolded or encrypted).
    /// The extend path then drops the index, as the kernel does, rather than
    /// append a block the index does not route to.
    #[allow(clippy::too_many_arguments)]
    fn buffer_add_dir_entry_indexed(
        &self,
        buf: &mut BlockBuffer,
        parent_ino: u32,
        parent_inode: &Inode,
        name: &[u8],
        target_ino: u32,
        file_type: crate::dir::DirEntryType,
        has_ft: bool,
    ) -> Result<()> {
        if parent_inode.flags & (EXT4_CASEFOLD_FL | EXT4_ENCRYPT_FL) != 0 {
            return Err(Error::OutOfBounds);
        }
        let mut read_logical = |logical: u64| -> Result<Vec<u8>> {
            let phys = self
                .map_inode_logical(parent_inode, logical)?
                .ok_or(Error::CorruptDirEntry("htree block is not mapped"))?;
            Ok(buf.get_mut(self, phys)?.to_vec())
        };
        // Every index block that routes this write is checked before it
        // is believed: a corrupt root or node sends the entry to the
        // wrong leaf, which then reads as a name the index cannot find.
        let root = read_logical(0)?;
        self.check_dx_block(parent_ino, parent_inode, &root, true)?;
        let leaf = crate::htree::lookup_leaf_with(
            name,
            &root,
            &self.sb.hash_seed,
            self.sb.unsigned_hash(),
            |logical| {
                let node = read_logical(u64::from(logical))?;
                self.check_dx_block(parent_ino, parent_inode, &node, false)?;
                Ok(node)
            },
        )?
        .ok_or(Error::CorruptDirEntry("htree root has no entries"))?;
        if leaf == 0 {
            return Err(Error::CorruptDirEntry(
                "htree routes a name to its own root",
            ));
        }
        let phys = self
            .map_inode_logical(parent_inode, u64::from(leaf))?
            .ok_or(Error::CorruptDirEntry("htree leaf is not mapped"))?;
        let block = buf.get_mut(self, phys)?;
        let reserved_tail = if self.csum.enabled && crate::dir::has_csum_tail(block) {
            12
        } else {
            0
        };
        crate::dir::add_entry_to_block(block, target_ino, name, file_type, has_ft, reserved_tail)?;
        if self.csum.enabled && reserved_tail == 12 {
            self.csum
                .patch_dir_entry_tail(parent_ino, parent_inode.generation, block);
        }
        Ok(())
    }

    /// Refuse an htree root (`root`) or interior node whose `dx_tail`
    /// checksum does not match, before it is used or rewritten.
    ///
    /// Restamping a block, or routing a write by it, without this blessed
    /// whatever corruption it held with a fresh checksum (Greptile on #196).
    /// No-op without metadata_csum, or for a block with no tail to check.
    fn check_dx_block(&self, dir_ino: u32, dir: &Inode, block: &[u8], root: bool) -> Result<()> {
        let count_offset = if root {
            // `dx_root_info` starts at 24; its length is the byte at 29,
            // and the format fixes it at 8. Anything else is a root whose
            // count and limit this would read from the wrong place, so it
            // is refused before its checksum is trusted or it is written
            // through, as the kernel refuses it (CodeRabbit on #196).
            match block.get(29) {
                Some(8) => 32,
                _ => return Err(Error::Corrupt("htree root info_length is not 8")),
            }
        } else {
            8
        };
        match self
            .csum
            .verify_dx_tail(dir_ino, dir.generation, block, count_offset)
        {
            Some(false) => Err(Error::BadChecksum {
                what: "htree index block",
            }),
            _ => Ok(()),
        }
    }

    /// Turn an indexed directory back into a linear one, which is what the
    /// kernel's `ext4_add_entry` does when it cannot insert through the
    /// index (`dx_fallback`).
    ///
    /// Every leaf is already an ordinary directory block, and so, read
    /// linearly, is the root (`.`, then a `..` spanning the rest) and each
    /// interior node (one unused record spanning the block). Only the
    /// inode's flag has to go. With metadata_csum a linear block must also
    /// end in a dirent tail, which those blocks do not, so the spanning
    /// record is shortened by twelve bytes and a tail written after it.
    ///
    /// The kernel refuses this on metadata_csum volumes because it cannot
    /// trust a broken index; here the index is intact and the tails are
    /// rebuilt.
    ///
    /// Staged into `buf`, the caller's open transaction, so the drop lands
    /// in the same commit as the create, link or rename that forced it
    /// (#347). It used to be written straight to the device after an early
    /// commit of `buf`, and a cut inside it left a half-converted index, or
    /// an inode the directory never gained a name for.
    fn buffer_drop_htree_index(&self, buf: &mut BlockBuffer, dir_ino: u32) -> Result<()> {
        let (inode, mut raw) = self.buffered_inode_verified(buf, dir_ino)?;
        if inode.flags & crate::inode::InodeFlags::INDEX.bits() == 0 {
            return Ok(());
        }
        let bs = self.sb.block_size() as usize;
        let physical = |logical: u64| {
            self.map_inode_logical(&inode, logical)?
                .ok_or(Error::CorruptDirEntry("htree block is not mapped"))
        };

        // The root and every interior node, by physical block, each
        // checked before it is converted and re-tailed.
        let root_phys = physical(0)?;
        let root = self.buffered_block(buf, root_phys)?;
        self.check_dx_block(dir_ino, &inode, &root, true)?;
        let mut nodes = Vec::new();
        if let (Ok(info), Ok((_, entries))) = (
            crate::htree::parse_root_info(&root),
            crate::htree::parse_root_entries(&root),
        ) {
            let mut level: Vec<u32> = entries.iter().map(|e| e.block).collect();
            for _ in 0..info.indirect_levels {
                let mut next = Vec::new();
                for logical in level {
                    let phys = physical(u64::from(logical))?;
                    let block = self.buffered_block(buf, phys)?;
                    self.check_dx_block(dir_ino, &inode, &block, false)?;
                    let (_, entries) = crate::htree::parse_node_entries(&block)?;
                    next.extend(entries.iter().map(|e| e.block));
                    nodes.push(phys);
                }
                level = next;
            }
        }

        if self.csum.enabled {
            // Each block, with the offset of the record that spans to its end.
            let spanning =
                std::iter::once((root_phys, 12)).chain(nodes.into_iter().map(|p| (p, 0)));
            for (phys, at) in spanning {
                let block = buf.get_mut(self, phys)?;
                block[at + 4..at + 6].copy_from_slice(&((bs - at - 12) as u16).to_le_bytes());
                self.csum
                    .patch_dir_entry_tail(dir_ino, inode.generation, block);
            }
        }

        let flags = inode.flags & !crate::inode::InodeFlags::INDEX.bits();
        raw[0x20..0x24].copy_from_slice(&flags.to_le_bytes());
        self.buffer_write_dir_inode(buf, dir_ino, inode.generation, &mut raw)
    }

    /// Commit a `BlockBuffer` atomically. Routes through the journal
    /// writer when one is available (crash-safe four-fence protocol);
    /// falls back to direct device writes + flush otherwise.
    ///
    /// In journaled mode, writes go to the **journal log** on disk and
    /// are then checkpointed to their final locations before the journal
    /// writer's `commit` returns, so later reads — the allocators' bitmap
    /// scans included — see the committed bytes from the device or from
    /// the cache's clean, write-through entries. No block is pinned.
    pub(crate) fn commit_block_buffer(&self, buf: BlockBuffer) -> Result<()> {
        if buf.dirty.is_empty() {
            return Ok(());
        }
        let cleared = buf.uninit_cleared.clone();
        let publish = |fs: &Self| fs.uninit_cleared.lock().unwrap().publish(cleared);
        if let Some(jw_mu) = &self.journal {
            let mut jw = jw_mu.lock().map_err(|_| {
                Error::Corrupt("journal writer mutex poisoned (prior write panicked)")
            })?;
            let mut tx = jw.begin();
            for (block, bytes) in &buf.dirty {
                tx.add_write(*block, bytes.clone())?;
            }
            jw.commit(self.dev.as_ref(), &tx)?;
            // Nothing is pinned: `commit` checkpoints every block to its
            // final location before it returns, through this same cache,
            // whose write-through leaves each one a clean LRU entry that
            // matches the device. Pinning them as well held every block a
            // write had ever touched until unmount, whatever the capacity
            // (#328).
            publish(self);
            Ok(())
        } else {
            let descriptors: std::collections::BTreeSet<u64> = buf
                .uninit_cleared
                .keys()
                .map(|&gi| self.sb.descriptor_location(gi as u64).0)
                .collect();
            if let Err(e) = self.write_direct(buf.dirty, &descriptors) {
                // Part of the commit may be on disk and none of it is in
                // `uninit_cleared`: stop this mount writing (#319).
                self.direct_commit_failed
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                return Err(e);
            }
            publish(self);
            Ok(())
        }
    }

    /// Write a committed buffer straight to the device, in three flushed
    /// stages, for a mount with no journal.
    ///
    /// 1. **Everything else** -- bitmaps, inode tables, extent and directory
    ///    blocks, data.
    /// 2. **The descriptor blocks that clear an uninit flag** (`descriptors`).
    ///    The map iterates by block number and the descriptor table sits
    ///    near the front of the volume, so it went out before the bitmap it
    ///    vouches for. A failure in between left `BLOCK_UNINIT` down over a
    ///    bitmap still holding whatever the flag licensed leaving there, and
    ///    the next mount trusted it (#319). Written after the bitmaps, a
    ///    failure anywhere earlier leaves the flag standing, and the flag
    ///    tells every reader to ignore the bitmap.
    /// 3. **The superblock, last.** It carries the markers that say work is
    ///    finished -- `s_last_orphan` cleared, the free counts credited --
    ///    and block 0 came first. A crash after it and before the inode
    ///    table left an orphan still allocated, still holding its blocks,
    ///    and named by nothing, so no later mount would retry it (#124).
    ///    Written last, a crash anywhere before it leaves the old
    ///    superblock, whose chain head sends the next mount back to finish.
    fn write_direct(
        &self,
        mut dirty: std::collections::BTreeMap<u64, Vec<u8>>,
        descriptors: &std::collections::BTreeSet<u64>,
    ) -> Result<()> {
        let bs = self.sb.block_size() as u64;
        let sb_block = crate::superblock::SUPERBLOCK_OFFSET / bs;
        let superblock = dirty.remove(&sb_block);
        let (late, early): (Vec<_>, Vec<_>) = dirty
            .into_iter()
            .partition(|(block, _)| descriptors.contains(block));
        for stage in [
            early,
            late,
            superblock.map(|b| (sb_block, b)).into_iter().collect(),
        ] {
            if stage.is_empty() {
                continue;
            }
            for (block, bytes) in stage {
                self.dev.write_at(block * bs, &bytes)?;
            }
            self.dev.flush()?;
        }
        Ok(())
    }

    /// Change the owner of `path` to (`uid`, `gid`). Both values are full
    /// 32-bit — the inode stores them as hi+lo u16 halves at different
    /// offsets per the ext4 on-disk format. Passing `u32::MAX` for either
    /// field leaves that value untouched (Linux lchown(2) convention).
    ///
    /// Updates `i_ctime = now` and recomputes the inode checksum on
    /// csum-enabled mounts.
    pub fn apply_chown(&self, path: &str, uid: u32, gid: u32) -> Result<()> {
        self.apply_chown_bytes(path.as_bytes(), uid, gid)
    }

    /// [`apply_chown`](Self::apply_chown) of a path given as bytes, never decoded.
    pub(crate) fn apply_chown_bytes(&self, path: &[u8], uid: u32, gid: u32) -> Result<()> {
        self.refuse_write()?;
        let ino = self.resolve(path)?;
        self.apply_chown_ino(ino, uid, gid)
    }

    /// [`apply_chown`](Self::apply_chown) on the inode `r` names.
    pub fn apply_chown_ino(&self, r: impl Into<InodeRef>, uid: u32, gid: u32) -> Result<()> {
        self.refuse_write()?;
        let r = r.into();
        let ino = r.ino;
        let (inode, mut raw) = self.live_inode(r)?;

        if uid != u32::MAX {
            let lo = (uid & 0xFFFF) as u16;
            let hi = ((uid >> 16) & 0xFFFF) as u16;
            raw[0x02..0x04].copy_from_slice(&lo.to_le_bytes());
            raw[0x78..0x7A].copy_from_slice(&hi.to_le_bytes());
        }
        if gid != u32::MAX {
            let lo = (gid & 0xFFFF) as u16;
            let hi = ((gid >> 16) & 0xFFFF) as u16;
            raw[0x18..0x1A].copy_from_slice(&lo.to_le_bytes());
            raw[0x7A..0x7C].copy_from_slice(&hi.to_le_bytes());
        }

        let now = self.runtime.now_unix_seconds();
        set_inode_time(&mut raw, InodeTime::Ctime, now);

        self.finalize_inode_raw(ino, inode.generation, &mut raw)?;
        self.commit_inode_write(ino, &raw)
    }

    /// Set the `i_flags` field (FS_IOC_SETFLAGS) for the inode at `path`.
    ///
    /// Bumps ctime. Fails with `Error::ReadOnly` on read-only mounts, or
    /// `Error::InvalidArgument` if `flags` changes any bit outside
    /// [`crate::inode::USER_MODIFIABLE_FLAGS`] — `INDEX`, `EXTENTS`,
    /// `INLINE_DATA`, `HUGE_FILE`, `ENCRYPT`, `VERITY`, `CASEFOLD` and the
    /// like describe how the inode's existing bytes are read, so flipping
    /// one without rewriting them would corrupt the file. Bits outside the
    /// mask that are already set may be passed back unchanged.
    pub fn apply_set_flags(&self, path: &str, flags: u32) -> Result<()> {
        self.apply_set_flags_bytes(path.as_bytes(), flags)
    }

    /// [`apply_set_flags`](Self::apply_set_flags) of a path given as bytes, never decoded.
    pub(crate) fn apply_set_flags_bytes(&self, path: &[u8], flags: u32) -> Result<()> {
        use crate::inode::{OFF_FLAGS, USER_MODIFIABLE_FLAGS};
        self.refuse_write()?;
        let ino = self.resolve(path)?;
        let (inode, mut raw) = self.read_inode_verified(ino)?;

        if (flags ^ inode.flags) & !USER_MODIFIABLE_FLAGS != 0 {
            return Err(Error::InvalidArgument(
                "set_flags: only the user-modifiable inode flags may change \
                 (INDEX, EXTENTS, INLINE_DATA, HUGE_FILE, ENCRYPT, VERITY, CASEFOLD and the like are managed)",
            ));
        }

        raw[OFF_FLAGS..OFF_FLAGS + 4].copy_from_slice(&flags.to_le_bytes());

        let now = self.runtime.now_unix_seconds();
        set_inode_time(&mut raw, InodeTime::Ctime, now);

        self.finalize_inode_raw(ino, inode.generation, &mut raw)?;
        self.commit_inode_write(ino, &raw)
    }

    /// Remove the extended attribute named `name` from the inode at `path`.
    /// `name` must carry a known namespace prefix (e.g. `"user.color"`).
    ///
    /// v1 scope: **in-inode xattrs only.** The in-inode region (bytes
    /// between `128 + i_extra_isize` and the end of the on-disk inode)
    /// is decoded, the matching entry is dropped, and the region is
    /// re-encoded in place. External xattr blocks (pointed at by
    /// Search the in-inode region first, then the external xattr block. If
    /// the external block becomes empty after removal, free it and zero
    /// `i_file_acl` (matches kernel behavior — empty xattr blocks are
    /// reaped on the spot rather than left dangling).
    ///
    /// Returns:
    /// - `Ok(())` on success.
    /// - `Error::NotFound` if the entry isn't present in either region.
    /// - `Error::InvalidArgument` on namespace-prefix issues.
    pub fn apply_removexattr(&self, path: &str, name: &str) -> Result<()> {
        self.apply_removexattr_bytes(path.as_bytes(), name)
    }

    /// [`apply_removexattr`](Self::apply_removexattr) of a path given as bytes, never decoded.
    pub(crate) fn apply_removexattr_bytes(&self, path: &[u8], name: &str) -> Result<()> {
        self.refuse_write()?;
        let ino = self.resolve(path)?;
        let (inode, mut raw) = self.read_inode_verified(ino)?;

        // Locate the in-inode xattr region (starts at 128 + i_extra_isize).
        let inode_size = self.sb.inode_size as usize;
        let i_extra_isize = if raw.len() >= 0x82 {
            u16::from_le_bytes(raw[0x80..0x82].try_into().unwrap()) as usize
        } else {
            0
        };
        let region_start = 128 + i_extra_isize;
        let region_end = inode_size.min(raw.len());

        // FROM BOTH PLACES. A name lives in the inode or in the external
        // block, and a set that moved it used to leave a copy in the other
        // (#377); removing only the first copy found then brought the
        // stale one back. Every copy goes, in one transaction.
        let mut found = false;
        // `i_extra_isize = 0` means no in-inode area at all: the kernel
        // does not parse one there (#380).
        if i_extra_isize != 0 && region_start + 4 <= region_end {
            let region = &mut raw[region_start..region_end];
            if crate::xattr::plan_remove_in_inode_region(region, name)?
                == crate::xattr::RemoveOutcome::Removed
            {
                found = true;
            }
        }
        let mut buf = BlockBuffer::new(self.sb.block_size());
        if self.buffer_remove_from_external_block(&mut buf, ino, &inode, &mut raw, name)? {
            found = true;
        }
        if !found {
            return Err(Error::NotFound);
        }
        // POSIX stamps ctime on an attribute write. It goes through
        // set_inode_time so the epoch bits are written too: the clock is
        // an i64 since #324, and its low four bytes alone are a time that
        // reads back as 1901 from 2038 (#386's neighbour, #324).
        set_inode_time(&mut raw, InodeTime::Ctime, self.runtime.now_unix_seconds());
        self.finalize_inode_raw(ino, inode.generation, &mut raw)?;
        self.buffer_write_inode(&mut buf, ino, &raw)?;
        self.commit_block_buffer(buf)
    }

    /// Remove `name` from `ino`'s external xattr block, staged in `buf`.
    /// Returns whether the name was there.
    ///
    /// `raw` is the inode image the caller will write in the same
    /// transaction: when the block is shared it gets a block of its own
    /// (`i_file_acl` moves), and when the block empties it is released
    /// (`i_file_acl` cleared, `i_blocks` one block lower). The caller
    /// re-checksums and stages the inode.
    fn buffer_remove_from_external_block(
        &self,
        buf: &mut BlockBuffer,
        ino: u32,
        inode: &crate::inode::Inode,
        raw: &mut [u8],
        name: &str,
    ) -> Result<bool> {
        if inode.file_acl == 0 {
            return Ok(false);
        }
        let block_nr = inode.file_acl;
        let mut block = self.buffer_read_xattr_block(buf, block_nr)?;
        let refs = u32::from_le_bytes(block[4..8].try_into().unwrap());
        match crate::xattr::plan_remove_from_external_block(&mut block, name, 1)? {
            crate::xattr::BlockRemoveOutcome::NotFound => Ok(false),
            crate::xattr::BlockRemoveOutcome::Removed if refs > 1 => {
                // Shared: this inode's remaining attributes move to a
                // block of its own, and the others keep the old one.
                let new_nr = self.buffer_unshare_xattr_block(buf, ino, block_nr, block)?;
                Self::write_file_acl(raw, new_nr)?;
                Ok(true)
            }
            crate::xattr::BlockRemoveOutcome::Removed => {
                if self.csum.enabled {
                    self.csum.patch_xattr_block(block_nr, &mut block);
                }
                buf.put(block_nr, block);
                Ok(true)
            }
            crate::xattr::BlockRemoveOutcome::RemovedNowEmpty => {
                // Freed only if no other inode shares it. The bitmap, the
                // descriptor and the superblock count move in the same
                // transaction as the inode that stops pointing at it.
                let freed = self.buffer_release_xattr_block(buf, block_nr)?;
                self.buffer_patch_sb_counters(buf, freed as i64, 0)?;
                // Both halves, at the offsets the reader uses.
                Self::write_file_acl(raw, 0)?;
                let sectors_per_block = self.sb.block_size() as u64 / 512;
                let new_blocks = inode.blocks.saturating_sub(sectors_per_block);
                Self::patch_inode_size_and_blocks(raw, inode.size, new_blocks)?;
                Ok(true)
            }
        }
    }

    /// Set (create or replace) the extended attribute `name` with `value`
    /// on the inode at `path`. `name` must carry a known namespace prefix
    /// (e.g. `"user.com.apple.FinderInfo"`).
    ///
    /// Try-order, matching the kernel:
    /// 1. **In-inode region** — between `128 + i_extra_isize` and the end
    ///    of the on-disk inode. Cheapest; no extra block.
    /// 2. **External xattr block** — when in-inode is full, fall back to a
    ///    dedicated block referenced by `i_file_acl`. Allocates a fresh
    ///    block when none exists, otherwise rewrites the existing one.
    ///    Returns `Error::NoSpaceLeftOnDevice` if even a full block can't
    ///    hold the new layout.
    pub fn apply_setxattr(&self, path: &str, name: &str, value: &[u8]) -> Result<()> {
        self.apply_setxattr_bytes(path.as_bytes(), name, value)
    }

    /// [`apply_setxattr`](Self::apply_setxattr) of a path given as bytes, never decoded.
    pub(crate) fn apply_setxattr_bytes(&self, path: &[u8], name: &str, value: &[u8]) -> Result<()> {
        self.refuse_write()?;
        let ino = self.resolve(path)?;
        let (inode, mut raw) = self.read_inode_verified(ino)?;

        let inode_size = self.sb.inode_size as usize;
        let mut i_extra_isize = if raw.len() >= 0x82 {
            u16::from_le_bytes(raw[0x80..0x82].try_into().unwrap()) as usize
        } else {
            0
        };
        // NO AREA AT 0x80 (#380). `i_extra_isize = 0` -- `ext2.ko`, older
        // kernels, 256-byte-inode ext2 volumes -- is an inode whose extra
        // fields are unused, and the kernel parses no in-inode xattr area
        // there: it reads 0x80.. as `i_extra_isize`, `i_checksum_hi` and the
        // `i_*_extra` words. An area written at 0x80 was invisible to it,
        // and on `metadata_csum` the checksum's high half was then stored
        // over the area's magic. The kernel gives such an inode its extra
        // fields, zeroed, before it writes an attribute
        // (`__ext4_expand_extra_isize`); so does this, in the same write.
        let extra = crate::inode::EXTRA_ISIZE_DEFAULT as usize;
        if i_extra_isize == 0 && inode_size.min(raw.len()) >= 128 + extra {
            raw[0x80..0x80 + extra].fill(0);
            write_inode_extra_isize(&mut raw);
            i_extra_isize = extra;
        }
        let region_start = 128 + i_extra_isize;
        let region_end = inode_size.min(raw.len());
        let inline_capable = i_extra_isize != 0 && region_start + 8 <= region_end;

        // Try in-inode first; on overflow fall through to the external block.
        let inline_result = if inline_capable {
            let region = &mut raw[region_start..region_end];
            crate::xattr::plan_set_in_inode_region(region, name, value)
        } else {
            Err(Error::NoSpaceLeftOnDevice)
        };

        // ONE COPY, WHEREVER IT LANDS (#377). The name may already live in
        // the other place: a value that grew past the inode, or shrank back
        // into it. That copy is removed in the same transaction, as the
        // kernel's `ext4_xattr_set_handle` does; left behind, the reader
        // returned whichever copy it met first -- the in-inode one, stale
        // or not -- and a remove brought the other back.
        match inline_result {
            Ok(_) => {
                // In-inode rewrite already in `raw`.
                let mut buf = BlockBuffer::new(self.sb.block_size());
                if self.buffer_remove_from_external_block(&mut buf, ino, &inode, &mut raw, name)? {
                    set_inode_time(&mut raw, InodeTime::Ctime, self.runtime.now_unix_seconds());
                }
                self.finalize_inode_raw(ino, inode.generation, &mut raw)?;
                self.buffer_write_inode(&mut buf, ino, &raw)?;
                self.commit_block_buffer(buf)
            }
            Err(Error::NoSpaceLeftOnDevice) => {
                // The set never re-encoded the region, so an old in-inode
                // copy is still there; it goes before the block gets the
                // new one, and `raw` carries that into the same commit.
                if inline_capable {
                    let region = &mut raw[region_start..region_end];
                    crate::xattr::plan_remove_in_inode_region(region, name)?;
                }
                self.apply_setxattr_external_block(ino, &inode, &mut raw, name, value)
            }
            Err(e) => Err(e),
        }
    }

    /// Recompute the inode checksum (when enabled) and splice both halves
    /// back into the inode image. No-op when csum disabled.
    pub(crate) fn finalize_inode_raw(
        &self,
        ino: u32,
        generation: u32,
        raw: &mut [u8],
    ) -> Result<()> {
        if self.csum.enabled {
            self.csum.patch_inode_checksum(ino, generation, raw);
        }
        Ok(())
    }

    /// Helper: route a setxattr that overflowed the in-inode region to the
    /// external xattr block. Either rewrites the existing block (when
    /// `i_file_acl != 0`) or allocates a fresh one.
    fn apply_setxattr_external_block(
        &self,
        ino: u32,
        inode: &crate::inode::Inode,
        raw: &mut [u8],
        name: &str,
        value: &[u8],
    ) -> Result<()> {
        let bs = self.sb.block_size();
        let bs_u64 = bs as u64;

        // Multi-block transaction: xattr block bytes + (alloc-side bitmap +
        // BGD + SB when fresh-block) + inode body. Atomic across the op.
        let mut buf = BlockBuffer::new(bs);
        // An xattr block on a volume without COMPAT_EXT_ATTR is one e2fsck
        // ignores: it clears i_file_acl and frees the block. The kernel's
        // `ext4_xattr_update_super_block` sets the feature on the first
        // xattr; so does this, in the same transaction (#88). This crate's
        // mkfs does not set it.
        if self.sb.feature_compat & features::Compat::EXT_ATTR.bits() == 0 {
            self.buffer_patch_sb_compat(&mut buf, features::Compat::EXT_ATTR.bits())?;
        }

        // Path A: existing external block — rewrite in-buffer, re-checksum.
        if inode.file_acl != 0 {
            let block_nr = inode.file_acl;
            let mut block = self.buffer_read_xattr_block(&mut buf, block_nr)?;
            let refs = u32::from_le_bytes(block[4..8].try_into().unwrap());
            crate::xattr::plan_set_in_external_block(&mut block, name, value, 1)?;
            if refs > 1 {
                // Shared: the edit is this inode's alone, so it gets its
                // own block. One block before and after, so i_blocks holds.
                let new_nr = self.buffer_unshare_xattr_block(&mut buf, ino, block_nr, block)?;
                Self::write_file_acl(raw, new_nr)?;
            } else {
                if self.csum.enabled {
                    self.csum.patch_xattr_block(block_nr, &mut block);
                }
                buf.put(block_nr, block);
            }
            // i_blocks unchanged — only need to bump ctime.
            let now = self.runtime.now_unix_seconds();
            set_inode_time(raw, InodeTime::Ctime, now);
            self.finalize_inode_raw(ino, inode.generation, raw)?;
            self.buffer_write_inode(&mut buf, ino, raw)?;
            return self.commit_block_buffer(buf);
        }

        // Path B: no external block yet — allocate, build, stage, then
        // point i_file_acl + i_blocks at it.
        let mut bitmap_reader = |block: u64| self.read_block(block);
        let inode_group = (ino - 1) / self.sb.inodes_per_group;
        let plan = crate::alloc::plan_block_allocation(
            &self.sb,
            &self.allocation_groups(),
            1,
            inode_group,
            &mut bitmap_reader,
        )?;
        let block_nr = plan.first_block;

        let mut block = vec![0u8; bs as usize];
        crate::xattr::plan_set_in_external_block(&mut block, name, value, 1)?;
        if self.csum.enabled {
            self.csum.patch_xattr_block(block_nr, &mut block);
        }
        buf.put(block_nr, block);

        // Stage allocator side-effects in the buffer.
        self.buffer_mark_block_run_used(&mut buf, block_nr, 1)?;
        self.buffer_patch_bgd_counters(
            &mut buf,
            plan.bgd.group_idx as usize,
            plan.bgd.free_blocks_delta,
            plan.bgd.free_inodes_delta,
            plan.bgd.used_dirs_delta,
        )?;
        self.buffer_patch_sb_counters(
            &mut buf,
            plan.sb.free_blocks_delta,
            plan.sb.free_inodes_delta,
        )?;

        // Splice block_nr into the inode: i_file_acl_lo at 0x68..0x6C, hi at
        // 0x76..0x78. The comment here used to say 0x74, and so did the
        // code, so a reader checking one against the other agreed.
        Self::write_file_acl(raw, block_nr)?;
        // Bump i_blocks by sectors_per_block (the xattr block now belongs
        // to this inode for du purposes).
        let sectors_per_block = bs_u64 / 512;
        let new_blocks = inode.blocks.saturating_add(sectors_per_block);
        Self::patch_inode_size_and_blocks(raw, inode.size, new_blocks)?;
        let now = self.runtime.now_unix_seconds();
        set_inode_time(raw, InodeTime::Ctime, now);
        self.finalize_inode_raw(ino, inode.generation, raw)?;
        self.buffer_write_inode(&mut buf, ino, raw)?;

        self.commit_block_buffer(buf)
    }

    /// Allocate one block near `ino`'s group, staged in `buf` with its
    /// bitmap, descriptor and superblock counts.
    ///
    /// Planned through the buffer, uninit clears included: a punch's tree
    /// repack calls this more than once in one transaction, and a second
    /// plan that still saw the group as BLOCK_UNINIT rebuilt its bitmap from
    /// metadata and handed out the block the first call staged (#291).
    fn buffer_allocate_block(&self, buf: &mut BlockBuffer, ino: u32) -> Result<u64> {
        let plan =
            self.plan_buffered_block_allocation(buf, 1, (ino - 1) / self.sb.inodes_per_group)?;
        self.buffer_mark_block_run_used(buf, plan.first_block, 1)?;
        self.buffer_patch_bgd_counters(
            buf,
            plan.bgd.group_idx as usize,
            plan.bgd.free_blocks_delta,
            plan.bgd.free_inodes_delta,
            plan.bgd.used_dirs_delta,
        )?;
        self.buffer_patch_sb_counters(buf, plan.sb.free_blocks_delta, plan.sb.free_inodes_delta)?;
        Ok(plan.first_block)
    }

    /// Drop one inode's reference to the external xattr block at `block_nr`.
    ///
    /// THE BLOCK MAY NOT BE THIS INODE'S ALONE. The kernel keeps one block
    /// for every inode with an identical attribute set and counts them in
    /// `h_refcount`, so files with the same ACL or security label share
    /// one. At a count of one the block is freed. Above one, the count is
    /// written back one lower and the block stays where the other inodes
    /// point. Returns the blocks freed, 0 or 1; the caller credits the
    /// superblock.
    pub(crate) fn buffer_release_xattr_block(
        &self,
        buf: &mut BlockBuffer,
        block_nr: u64,
    ) -> Result<u64> {
        let mut block = self.buffer_read_xattr_block(buf, block_nr)?;
        let refs = u32::from_le_bytes(block[4..8].try_into().unwrap());
        if refs <= 1 {
            return self.buffer_free_block_run_and_bgd(buf, block_nr, 1);
        }
        block[4..8].copy_from_slice(&(refs - 1).to_le_bytes());
        if self.csum.enabled {
            self.csum.patch_xattr_block(block_nr, &mut block);
        }
        buf.put(block_nr, block);
        Ok(0)
    }

    /// The external xattr block at `block_nr`, read for an edit through
    /// `buf`, and refused when it is not one.
    ///
    /// NOTHING IS WRITTEN OVER A BLOCK THAT HAS NOT BEEN CHECKED (#378).
    /// Every edit used to read the block named by `i_file_acl` and go
    /// ahead: a block without the magic was formatted as an empty xattr
    /// block over whatever it held -- another file's data, a directory --
    /// and a block whose checksum failed was edited and restamped, which
    /// blessed the corruption. `h_refcount` was decremented on the same
    /// trust. The kernel's `ext4_xattr_check_block` refuses all of them
    /// with EFSCORRUPTED, and so does this: see
    /// [`crate::xattr::check_external_block`].
    fn buffer_read_xattr_block(&self, buf: &mut BlockBuffer, block_nr: u64) -> Result<Vec<u8>> {
        let block = buf.get_mut(self, block_nr)?.clone();
        crate::xattr::check_external_block(&self.csum, block_nr, &block)?;
        Ok(block)
    }

    /// Give `ino` its own copy of a shared xattr block holding `block`'s
    /// edited contents: a new block for this inode, and one reference fewer
    /// on the old one. Returns the new block's number.
    fn buffer_unshare_xattr_block(
        &self,
        buf: &mut BlockBuffer,
        ino: u32,
        old_nr: u64,
        mut block: Vec<u8>,
    ) -> Result<u64> {
        let new_nr = self.buffer_allocate_block(buf, ino)?;
        block[4..8].copy_from_slice(&1u32.to_le_bytes());
        if self.csum.enabled {
            self.csum.patch_xattr_block(new_nr, &mut block);
        }
        buf.put(new_nr, block);
        self.buffer_release_xattr_block(buf, old_nr)?;
        Ok(new_nr)
    }

    /// `i_dtime` for an inode deleted now. It has no `*_extra` word and
    /// so no epoch bits: like the kernel, store the low 32 bits of the
    /// clock, unsigned.
    fn dtime_now(&self) -> u32 {
        self.runtime.now_unix_seconds() as u32
    }

    /// Set the access + modification times on `path`. Mirrors POSIX
    /// `utimensat(2)`: `atime_sec/nsec` and `mtime_sec/nsec` each replace
    /// the inode's atime/mtime. `ctime` is bumped to now (POSIX requires
    /// the change-time stamp on any attribute write). The [`TIME_OMIT`]
    /// sentinel on either `_sec` leaves that pair unchanged (lets callers
    /// touch just atime or just mtime).
    ///
    /// Each `_nsec` also takes `utimensat(2)`'s sentinels: [`UTIME_OMIT`]
    /// leaves that pair unchanged and [`UTIME_NOW`] sets it to the
    /// mount's current time, the `_sec` beside either being ignored. When
    /// both pairs are omitted nothing is written, ctime included, as on
    /// Linux. Any other `_nsec` of 1e9 or more is `InvalidArgument`,
    /// refused before anything is written.
    ///
    /// Seconds are signed and 64-bit because that is what the format
    /// means: the on-disk base is a signed 32-bit count, extended by the
    /// low two bits of the matching `*_extra` field. A `u32` here could
    /// not express a pre-1970 date at all, and stored every date past
    /// 2038 as one in the 1900s — the base was written and the epoch
    /// bits left zero, so the value read back 136 years early.
    ///
    /// `nsec` values are the sub-second timestamp in nanoseconds and are
    /// only written when the inode's `i_extra_isize` region is large
    /// enough to hold them (requires ≥ 160-byte inodes — the ext4 tooling
    /// default). That same region holds the epoch bits, so on an inode
    /// too small to carry it, a time needing them is refused rather than
    /// silently stored as the wrong century.
    pub fn apply_utimens(
        &self,
        path: &str,
        atime_sec: i64,
        atime_nsec: u32,
        mtime_sec: i64,
        mtime_nsec: u32,
    ) -> Result<()> {
        self.apply_utimens_bytes(
            path.as_bytes(),
            atime_sec,
            atime_nsec,
            mtime_sec,
            mtime_nsec,
        )
    }

    /// [`apply_utimens`](Self::apply_utimens) of a path given as bytes, never decoded.
    pub(crate) fn apply_utimens_bytes(
        &self,
        path: &[u8],
        atime_sec: i64,
        atime_nsec: u32,
        mtime_sec: i64,
        mtime_nsec: u32,
    ) -> Result<()> {
        self.refuse_write()?;
        // Before resolving, so an unstorable time or nanosecond count is
        // refused as EINVAL whether or not the path exists.
        TimeUpdate::check(atime_sec, atime_nsec)?;
        TimeUpdate::check(mtime_sec, mtime_nsec)?;
        let ino = self.resolve(path)?;
        self.apply_utimens_ino(ino, atime_sec, atime_nsec, mtime_sec, mtime_nsec)
    }

    /// [`apply_utimens`](Self::apply_utimens) on the inode `r` names.
    pub fn apply_utimens_ino(
        &self,
        r: impl Into<InodeRef>,
        atime_sec: i64,
        atime_nsec: u32,
        mtime_sec: i64,
        mtime_nsec: u32,
    ) -> Result<()> {
        self.refuse_write()?;
        // One reading of the clock serves UTIME_NOW and the ctime bump.
        let now = self.runtime.now_unix_seconds();
        let atime = TimeUpdate::resolve(atime_sec, atime_nsec, now)?;
        let mtime = TimeUpdate::resolve(mtime_sec, mtime_nsec, now)?;
        let r = r.into();
        let ino = r.ino;
        let (inode, mut raw) = self.live_inode(r)?;

        if let (TimeUpdate::Omit, TimeUpdate::Omit) = (atime, mtime) {
            return Ok(());
        }

        // Extra-isize region carries the nsec fields AND the epoch bits.
        // Offsets (relative to inode start):
        //   0x84 i_ctime_extra  (needs i_extra_isize ≥  8)
        //   0x88 i_mtime_extra  (needs i_extra_isize ≥ 12)
        //   0x8C i_atime_extra  (needs i_extra_isize ≥ 16)
        // Linux packs each as `(nsec << 2) | epoch_bits`.
        let i_extra_isize = if raw.len() >= 0x82 {
            u16::from_le_bytes(raw[0x80..0x82].try_into().unwrap())
        } else {
            0
        };
        let has_mtime_extra = i_extra_isize >= 12 && raw.len() >= 0x8C;
        let has_atime_extra = i_extra_isize >= 16 && raw.len() >= 0x90;

        // (base offset, extra offset if the inode has room for it)
        let fields = [
            (atime, 0x08, has_atime_extra.then_some(0x8C)),
            (mtime, 0x10, has_mtime_extra.then_some(0x88)),
        ];

        // Refuse before writing anything, so a rejected call leaves the
        // inode exactly as it was rather than half-updated.
        for (update, _, extra) in fields {
            if let TimeUpdate::Set(sec, _) = update {
                if crate::inode::encode_extra_time(sec).1 != 0 && extra.is_none() {
                    return Err(Error::InvalidArgument(
                        "timestamp past 2038 needs an *_extra field this inode is too small to hold",
                    ));
                }
            }
        }

        for (update, base_off, extra) in fields {
            let TimeUpdate::Set(sec, nsec) = update else {
                continue;
            };
            let (base, epoch) = crate::inode::encode_extra_time(sec);
            raw[base_off..base_off + 4].copy_from_slice(&base.to_le_bytes());
            if let Some(off) = extra {
                let packed = pack_nsec_lo(nsec) | epoch;
                raw[off..off + 4].copy_from_slice(&packed.to_le_bytes());
            }
        }

        // POSIX: any attribute write bumps ctime.
        // Its nanoseconds go to zero with it: `now` is whole seconds.
        set_inode_time(&mut raw, InodeTime::Ctime, now);

        self.finalize_inode_raw(ino, inode.generation, &mut raw)?;
        self.commit_inode_write(ino, &raw)
    }

    /// Unlink a regular file / symlink / special file at `path`.
    ///
    /// Semantics:
    /// - Refuses to unlink a directory (use a future `apply_rmdir`).
    /// - Decrements the target inode's `i_links_count`. When that reaches
    ///   zero, frees every data block via `plan_truncate_shrink(size → 0)`,
    ///   clears the inode bitmap bit, zeroes the inode body, and sets
    ///   `i_dtime = now`. When `links_count > 1` we only drop the dir entry
    ///   and decrement — matches POSIX unlink semantics for hard-linked files.
    /// - Mutates: parent-dir block (entry removal), target inode, block +
    ///   inode bitmaps, BGD counters, SB counters. No journaling yet —
    ///   safe only on scratch images (same caveat as `apply_truncate_shrink`).
    ///
    /// Returns `Error::NotFound` if the path doesn't exist,
    /// `Error::NotADirectory` if the parent isn't a directory, and
    /// `Error::IsADirectory` (POSIX EISDIR) if the target is a directory.
    pub fn apply_unlink(&self, path: &str) -> Result<()> {
        self.apply_unlink_bytes(path.as_bytes())
    }

    /// [`apply_unlink`](Self::apply_unlink) of a path given as bytes, never decoded.
    pub(crate) fn apply_unlink_bytes(&self, path: &[u8]) -> Result<()> {
        self.refuse_write()?;
        // POSIX: a trailing slash asserts the path refers to a directory,
        // which is incompatible with `unlink(2)` no matter what kind of file
        // the path resolves to. `split_parent_and_base` swallows the slash,
        // so snapshot the flag first.
        let trailing_slash = path.len() > 1 && path.ends_with(b"/");
        let (parent_path, base_name) = split_parent_and_base(path)?;
        let parent_ino = self.resolve(parent_path)?;
        if trailing_slash {
            // The call fails either way; only the errno depends on what the
            // name is: a directory is EISDIR, as unlink(2) says of any
            // directory, and anything else ENOTDIR, the slash having
            // asserted a directory.
            let (parent_inode, _) = self.live_dir(parent_ino.into())?;
            let target_ino = self.find_entry_in_dir(parent_ino, &parent_inode, base_name)?;
            let (target_inode, _) = self.live_inode(target_ino.into())?;
            return Err(if target_inode.is_dir() {
                Error::IsADirectory
            } else {
                Error::NotADirectory
            });
        }
        self.apply_unlink_at(parent_ino, base_name)
    }

    /// [`apply_unlink`](Self::apply_unlink) of entry `name` in directory
    /// `dir`.
    pub fn apply_unlink_at(&self, dir: impl Into<InodeRef>, name: &[u8]) -> Result<()> {
        self.refuse_write()?;
        check_entry_name(name)?;
        let dir = dir.into();
        let parent_ino_num = dir.ino;
        let (parent_inode, _parent_raw) = self.live_dir(dir)?;

        let target_ino = self.find_entry_in_dir(parent_ino_num, &parent_inode, name)?;
        let (target_inode, mut target_raw) = self.live_inode(target_ino.into())?;
        if target_inode.is_dir() {
            // POSIX: unlink(2) on a directory must fail with EISDIR; the
            // caller should use rmdir(2) instead.
            return Err(Error::IsADirectory);
        }

        // All mutations land in this buffer and commit as one transaction.
        let mut buf = BlockBuffer::new(self.sb.block_size());

        // Remove the dir entry from the parent. Scans each block until
        // `remove_entry_from_block` reports success.
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
        let bs = self.sb.block_size();
        let parent_blocks = parent_inode.size.div_ceil(bs as u64);
        let mut removed = false;
        for logical in 0..parent_blocks {
            let Some(phys) = self.map_inode_logical(&parent_inode, logical)? else {
                continue;
            };
            let block = buf.get_mut(self, phys)?;
            // An index block holds no entries to remove, and read as one it
            // ends in a tail-shaped dt_reserved and a `..` spanning the rest
            // (#233).
            if Self::is_htree_index_block(&parent_inode, logical, block) {
                continue;
            }
            // `dir_entry_tail` occupies the last 12 bytes when metadata_csum
            // is on; don't scribble over it.
            let reserved_tail = if self.csum.enabled && crate::dir::has_csum_tail(block) {
                12
            } else {
                0
            };
            if crate::dir::remove_entry_from_block(block, name, has_ft, reserved_tail)? {
                // Recompute the tail csum if present — entry-list shape changed.
                if self.csum.enabled && reserved_tail == 12 {
                    self.csum
                        .patch_dir_entry_tail(parent_ino_num, parent_inode.generation, block);
                }
                removed = true;
                break;
            }
        }
        if !removed {
            return Err(Error::NotFound);
        }

        // Decrement link count. Non-zero after → just persist the new count.
        let new_links = target_inode.links_count.saturating_sub(1);
        target_raw[0x1A..0x1C].copy_from_slice(&new_links.to_le_bytes());

        if new_links > 0 {
            self.finalize_inode_raw(target_ino, target_inode.generation, &mut target_raw)?;
            self.buffer_write_inode(&mut buf, target_ino, &target_raw)?;
            return self.commit_block_buffer(buf);
        }

        // Last link gone — free data blocks + inode slot, all into the same
        // transaction so a crash either keeps everything or undoes everything.
        let mut freed_sectors: u64 = 0;
        let sectors_per_block = bs as u64 / 512;
        if target_inode.has_extents() {
            let runs = self.extent_tree_runs(target_ino, &target_inode)?;
            freed_sectors += self.buffer_free_runs(&mut buf, &runs)? * sectors_per_block;
        } else {
            // A BLOCK-MAPPED FILE HAS BLOCKS TOO. Only extents were freed,
            // so every ext2/ext3-style file left its data and indirect
            // blocks allocated with nothing pointing at them.
            freed_sectors +=
                self.buffer_free_block_map(&mut buf, &target_inode)? * sectors_per_block;
        }

        // The xattr block goes with the inode, or loses one reference if
        // another inode shares it. It was never released at all.
        if target_inode.file_acl != 0 {
            freed_sectors += self.buffer_release_xattr_block(&mut buf, target_inode.file_acl)?
                * sectors_per_block;
        }

        // Inode bitmap + BGD free_inodes_count; SB counter for both
        // freed_blocks AND +1 inode goes via one buffer_patch_sb_counters
        // call below.
        self.buffer_free_inode_slot(&mut buf, target_ino)?;

        let freed_blocks = freed_sectors.checked_div(sectors_per_block).unwrap_or(0);
        self.buffer_patch_sb_counters(&mut buf, freed_blocks as i64, 1)?;

        // Zero the inode body. Kernel sets dtime = now, mode = 0, and
        // leaves the generation intact (helps tooling detect the dead slot).
        let inode_size = self.sb.inode_size as usize;
        let old_gen = target_inode.generation;
        for b in &mut target_raw[..inode_size] {
            *b = 0;
        }
        let dtime = self.dtime_now();
        target_raw[0x14..0x18].copy_from_slice(&dtime.to_le_bytes()); // dtime
        target_raw[0x64..0x68].copy_from_slice(&old_gen.to_le_bytes()); // generation
        self.finalize_inode_raw(target_ino, old_gen, &mut target_raw)?;
        self.buffer_write_inode(&mut buf, target_ino, &target_raw)?;

        self.commit_block_buffer(buf)
    }

    /// Common setup for creating a new inode inside a directory: resolves
    /// the parent, checks preconditions, allocates an inode, and stages the
    /// bitmap + counter updates into a fresh `BlockBuffer`. The caller then
    /// builds the inode bytes and adds the dir entry.
    fn plan_new_inode_in_dir(&self, dir: InodeRef, name: &[u8]) -> Result<NewInodePlan> {
        check_new_entry_name(name)?;
        let parent_ino = dir.ino;
        let (parent_inode, _) = self.live_dir(dir)?;
        if self.entry_exists(parent_ino, &parent_inode, name)? {
            return Err(Error::AlreadyExists);
        }

        let parent_group = (parent_ino - 1) / self.sb.inodes_per_group;
        let bs = self.sb.block_size();
        let mut bitmap_reader = |block: u64| self.read_block(block);
        let plan = crate::alloc::plan_inode_allocation(
            &self.sb,
            &self.allocation_groups(),
            false,
            parent_group,
            &mut bitmap_reader,
        )?;
        let new_ino = plan.inode;

        let mut buf = BlockBuffer::new(bs);
        self.buffer_mark_inode_used(&mut buf, new_ino)?;
        self.buffer_patch_bgd_counters(
            &mut buf,
            plan.bgd.group_idx as usize,
            plan.bgd.free_blocks_delta,
            plan.bgd.free_inodes_delta,
            plan.bgd.used_dirs_delta,
        )?;
        self.buffer_patch_sb_counters(
            &mut buf,
            plan.sb.free_blocks_delta,
            plan.sb.free_inodes_delta,
        )?;

        Ok(NewInodePlan {
            new_ino,
            parent_ino,
            parent_inode,
            buf,
        })
    }

    /// Create a new regular file at `path` with permission bits `mode`
    /// (e.g. `0o644`). Returns the allocated inode number on success.
    ///
    /// Semantics:
    /// - Parent must exist and be a directory.
    /// - Refuses if `path` already exists.
    /// - Allocates an inode via `plan_inode_allocation` (hints to the
    ///   parent's group), marks the bitmap, bumps BGD + SB counters.
    /// - Initialises the inode as a regular file with EXTENTS flag and an
    ///   empty extent tree (size=0, blocks=0). Timestamps set to `now`.
    /// - Adds the directory entry into the first parent block with room
    ///   (linear; htree-extending dirs are a follow-up).
    /// - Not journaled — scratch-image safe, same caveat as other Phase-4
    ///   applies.
    pub fn apply_create(&self, path: &str, mode: u16) -> Result<u32> {
        self.apply_create_bytes(path.as_bytes(), mode)
    }

    /// [`apply_create`](Self::apply_create) of a path given as bytes, never decoded.
    pub(crate) fn apply_create_bytes(&self, path: &[u8], mode: u16) -> Result<u32> {
        self.refuse_write()?;
        let (parent, name) = self.resolve_new_parent(path)?;
        self.apply_create_at(parent, name, mode)
    }

    /// [`apply_create`](Self::apply_create) of entry `name` in directory
    /// `dir`. `name` is bytes and need not be UTF-8.
    pub fn apply_create_at(&self, dir: impl Into<InodeRef>, name: &[u8], mode: u16) -> Result<u32> {
        self.refuse_write()?;
        let dir = dir.into();
        let NewInodePlan {
            new_ino,
            parent_ino,
            parent_inode,
            mut buf,
        } = self.plan_new_inode_in_dir(dir, name)?;

        let raw = self.build_regular_file_inode(new_ino, mode)?;
        self.buffer_write_inode(&mut buf, new_ino, &raw)?;

        // Multi-block transaction: inode bitmap + BGD + SB + new inode +
        // parent dir entry, all atomic, and the parent's growth with them
        // when it has no room (see `extend_dir_and_add_entry` for the one
        // case that commits early).
        match self.buffer_add_dir_entry_inplace(
            &mut buf,
            parent_ino,
            &parent_inode,
            name,
            new_ino,
            crate::dir::DirEntryType::RegFile,
        ) {
            Ok(()) => {
                self.commit_block_buffer(buf)?;
                Ok(new_ino)
            }
            Err(Error::OutOfBounds) => {
                // Parent dir is full → grow it in the same transaction as
                // the inode allocation.
                self.extend_dir_and_add_entry(
                    &mut buf,
                    parent_ino,
                    name,
                    new_ino,
                    crate::dir::DirEntryType::RegFile,
                )?;
                self.commit_block_buffer(buf)?;
                Ok(new_ino)
            }
            Err(e) => Err(e),
        }
    }

    /// Create a special file (FIFO, socket, char device, block device).
    /// `mode` must include the type bits (`S_IFIFO`, `S_IFSOCK`, `S_IFCHR`,
    /// or `S_IFBLK`) plus the permission bits. `major` and `minor` are the
    /// device numbers (both 0 for FIFOs and sockets). Mirrors POSIX `mknod`.
    pub fn apply_mknod(&self, path: &str, mode: u16, major: u32, minor: u32) -> Result<u32> {
        self.apply_mknod_bytes(path.as_bytes(), mode, major, minor)
    }

    /// [`apply_mknod`](Self::apply_mknod) of a path given as bytes, never decoded.
    pub(crate) fn apply_mknod_bytes(
        &self,
        path: &[u8],
        mode: u16,
        major: u32,
        minor: u32,
    ) -> Result<u32> {
        self.refuse_write()?;
        Self::mknod_entry_type(mode)?;
        let (parent, name) = self.resolve_new_parent(path)?;
        self.apply_mknod_at(parent, name, mode, major, minor)
    }

    /// The directory-entry type for a special file of `mode`, or the
    /// refusal of a mode that is not one.
    fn mknod_entry_type(mode: u16) -> Result<crate::dir::DirEntryType> {
        let file_type = mode & crate::inode::S_IFMT;
        match file_type {
            crate::inode::S_IFCHR
            | crate::inode::S_IFBLK
            | crate::inode::S_IFIFO
            | crate::inode::S_IFSOCK => Ok(crate::dir::DirEntryType::from_mode(file_type)),
            _ => Err(Error::InvalidArgument(
                "mknod: unsupported type; use create/mkdir for reg/dir",
            )),
        }
    }

    /// [`apply_mknod`](Self::apply_mknod) of entry `name` in directory
    /// `dir`.
    pub fn apply_mknod_at(
        &self,
        dir: impl Into<InodeRef>,
        name: &[u8],
        mode: u16,
        major: u32,
        minor: u32,
    ) -> Result<u32> {
        self.refuse_write()?;
        let dir = dir.into();
        let dir_entry_type = Self::mknod_entry_type(mode)?;
        let NewInodePlan {
            new_ino,
            parent_ino,
            parent_inode,
            mut buf,
        } = self.plan_new_inode_in_dir(dir, name)?;

        let raw = self.build_special_file_inode(new_ino, mode, major, minor)?;
        self.buffer_write_inode(&mut buf, new_ino, &raw)?;

        match self.buffer_add_dir_entry_inplace(
            &mut buf,
            parent_ino,
            &parent_inode,
            name,
            new_ino,
            dir_entry_type,
        ) {
            Ok(()) => {
                self.commit_block_buffer(buf)?;
                Ok(new_ino)
            }
            Err(Error::OutOfBounds) => {
                self.extend_dir_and_add_entry(&mut buf, parent_ino, name, new_ino, dir_entry_type)?;
                self.commit_block_buffer(buf)?;
                Ok(new_ino)
            }
            Err(e) => Err(e),
        }
    }

    /// Write inode checksum fields (lo at OFF_CHECKSUM_LO, hi at OFF_CHECKSUM_HI)
    /// when metadata checksums are enabled for this filesystem.
    fn stamp_inode_checksum(&self, raw: &mut [u8], ino: u32, generation: u32) {
        if self.csum.enabled {
            self.csum.patch_inode_checksum(ino, generation, raw);
        }
    }

    fn build_special_file_inode(
        &self,
        ino: u32,
        mode: u16,
        major: u32,
        minor: u32,
    ) -> Result<Vec<u8>> {
        use crate::inode::{OFF_BLOCK, OFF_LINKS_COUNT, OFF_MODE};
        let inode_size = self.sb.inode_size as usize;
        let mut raw = vec![0u8; inode_size];

        raw[OFF_MODE..OFF_MODE + 2].copy_from_slice(&mode.to_le_bytes());
        raw[OFF_LINKS_COUNT..OFF_LINKS_COUNT + 2].copy_from_slice(&1u16.to_le_bytes());

        // Device files: store encoded device number in i_block (no EXTENTS).
        // Linux stores old (i_block[0]) and new (i_block[1]) formats.
        let file_type = mode & crate::inode::S_IFMT;
        if file_type == crate::inode::S_IFBLK || file_type == crate::inode::S_IFCHR {
            let old_dev = (major << 8) | (minor & 0xff);
            raw[OFF_BLOCK..OFF_BLOCK + 4].copy_from_slice(&old_dev.to_le_bytes());
            let new_dev = (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12);
            raw[OFF_BLOCK + 4..OFF_BLOCK + 8].copy_from_slice(&new_dev.to_le_bytes());
        }

        // i_extra_isize first: it says whether the epoch bits have a home.
        write_inode_extra_isize(&mut raw);
        let now = self.runtime.now_unix_seconds();
        write_inode_timestamps(&mut raw, now);
        let generation = self.runtime.next_inode_generation();
        write_inode_generation(&mut raw, generation);
        self.stamp_inode_checksum(&mut raw, ino, generation);
        Ok(raw)
    }

    /// Create a symbolic link at `linkpath` whose target is `target`.
    /// Mirrors POSIX `symlink(target, linkpath)`: allocates a fresh inode
    /// with mode S_IFLNK, installs the target bytes, and adds a dir entry
    /// at the link path.
    ///
    /// Two storage paths:
    /// - **Fast symlink** (`target.len() <= 60`): target stored inline in
    ///   the 60-byte `i_block` area; no data-block allocation.
    /// - **Slow symlink** (`61..=255` bytes): one filesystem block is
    ///   allocated and the target is written there, with an EXTENTS
    ///   i_block pointing at it.
    ///
    /// POSIX caps symlink targets at SYMLINK_MAX (255 bytes on Linux +
    /// macOS). Longer returns `Error::NameTooLong` → ENAMETOOLONG.
    pub fn apply_symlink(&self, target: &str, linkpath: &str) -> Result<u32> {
        self.apply_symlink_bytes(target.as_bytes(), linkpath.as_bytes())
    }

    /// [`apply_symlink`](Self::apply_symlink) of a path given as bytes, never decoded.
    pub(crate) fn apply_symlink_bytes(&self, target: &[u8], linkpath: &[u8]) -> Result<u32> {
        self.refuse_write()?;
        self.check_symlink_target(target)?;
        let (parent, name) = self.resolve_new_parent(linkpath)?;
        self.apply_symlink_at(parent, name, target)
    }

    fn check_symlink_target(&self, target: &[u8]) -> Result<()> {
        if target.is_empty() {
            return Err(Error::InvalidArgument("symlink target is empty"));
        }
        // A C string cannot carry one, and readlink(2) would stop at it.
        if target.contains(&0) {
            return Err(Error::InvalidArgument(
                "a symlink target cannot contain a NUL byte",
            ));
        }
        // PATH_MAX cap (matches Linux). Slow path allocates exactly one fs
        // block, so we additionally require target.len() <= block_size — the
        // 4096 ceiling matches the typical ext4 block size and Linux PATH_MAX.
        let max_target = 4096usize.min(self.sb.block_size() as usize);
        if target.len() > max_target {
            return Err(Error::NameTooLong);
        }
        Ok(())
    }

    /// [`apply_symlink`](Self::apply_symlink): entry `name` in directory
    /// `dir`, pointing at `target`. Both are bytes and need not be UTF-8.
    pub fn apply_symlink_at(
        &self,
        dir: impl Into<InodeRef>,
        name: &[u8],
        target: &[u8],
    ) -> Result<u32> {
        self.refuse_write()?;
        let dir = dir.into();
        self.check_symlink_target(target)?;

        let NewInodePlan {
            new_ino,
            parent_ino,
            parent_inode,
            mut buf,
        } = self.plan_new_inode_in_dir(dir, name)?;

        let parent_group = (parent_ino - 1) / self.sb.inodes_per_group;
        let bs = self.sb.block_size();

        // Fast-symlink if target strictly fits inline (i_block is 60 bytes);
        // otherwise allocate a block and stage its bytes into the buffer.
        // Linux's `ext4_symlink` switches to the slow path when
        // `target.len() >= sizeof(i_block)` (i.e. >= 60), and our readlink
        // path mirrors that boundary, so we match here.
        let raw = if target.len() < 60 {
            self.build_fast_symlink_inode(new_ino, target)?
        } else {
            let mut bitmap_reader = |block: u64| self.read_block(block);
            let bplan = crate::alloc::plan_block_allocation(
                &self.sb,
                &self.allocation_groups(),
                1,
                parent_group,
                &mut bitmap_reader,
            )?;
            let data_phys = bplan.first_block;

            self.buffer_mark_block_run_used(&mut buf, data_phys, 1)?;
            self.buffer_patch_bgd_counters(
                &mut buf,
                bplan.bgd.group_idx as usize,
                bplan.bgd.free_blocks_delta,
                bplan.bgd.free_inodes_delta,
                bplan.bgd.used_dirs_delta,
            )?;
            self.buffer_patch_sb_counters(
                &mut buf,
                bplan.sb.free_blocks_delta,
                bplan.sb.free_inodes_delta,
            )?;

            let mut block = vec![0u8; bs as usize];
            block[..target.len()].copy_from_slice(target);
            buf.put(data_phys, block);

            self.build_slow_symlink_inode(new_ino, target, data_phys)?
        };
        self.buffer_write_inode(&mut buf, new_ino, &raw)?;

        match self.buffer_add_dir_entry_inplace(
            &mut buf,
            parent_ino,
            &parent_inode,
            name,
            new_ino,
            crate::dir::DirEntryType::Symlink,
        ) {
            Ok(()) => {
                self.commit_block_buffer(buf)?;
                Ok(new_ino)
            }
            Err(Error::OutOfBounds) => {
                self.extend_dir_and_add_entry(
                    &mut buf,
                    parent_ino,
                    name,
                    new_ino,
                    crate::dir::DirEntryType::Symlink,
                )?;
                self.commit_block_buffer(buf)?;
                Ok(new_ino)
            }
            Err(e) => Err(e),
        }
    }

    /// Compose a fresh fast-symlink inode image: `S_IFLNK | 0o777`, 1 link,
    /// `i_size = target.len()`, 0 blocks, NO EXTENTS flag (fast symlinks
    /// store their target directly in the 60-byte `i_block` area — no
    /// extent tree).
    fn build_fast_symlink_inode(&self, ino: u32, target: &[u8]) -> Result<Vec<u8>> {
        use crate::inode::{OFF_BLOCK, OFF_FLAGS, OFF_LINKS_COUNT, OFF_MODE, OFF_SIZE_LO};
        debug_assert!(target.len() < 60);
        let mut raw = vec![0u8; self.sb.inode_size as usize];

        // Symlinks are traditionally rwxrwxrwx — the OS enforces access on
        // the *target*, not the symlink itself.
        let mode_bits = crate::inode::S_IFLNK | 0o0777;
        raw[OFF_MODE..OFF_MODE + 2].copy_from_slice(&mode_bits.to_le_bytes());
        raw[OFF_SIZE_LO..OFF_SIZE_LO + 4].copy_from_slice(&(target.len() as u32).to_le_bytes());
        raw[OFF_LINKS_COUNT..OFF_LINKS_COUNT + 2].copy_from_slice(&1u16.to_le_bytes());
        // Fast symlinks store the target inline in the i_block area — no extent tree.
        raw[OFF_FLAGS..OFF_FLAGS + 4].copy_from_slice(&0u32.to_le_bytes());
        let inline_target_off = OFF_BLOCK;
        raw[inline_target_off..inline_target_off + target.len()].copy_from_slice(target);

        // i_extra_isize first: it says whether the epoch bits have a home.
        write_inode_extra_isize(&mut raw);
        let now = self.runtime.now_unix_seconds();
        write_inode_timestamps(&mut raw, now);
        let generation = self.runtime.next_inode_generation();
        write_inode_generation(&mut raw, generation);
        self.stamp_inode_checksum(&mut raw, ino, generation);
        Ok(raw)
    }

    /// Map logical block 0 of a new inode image to `phys`, in the volume's
    /// dialect: an EXTENTS_FL leaf with one extent on ext4, direct pointer 0
    /// on ext2/ext3. e2fsck calls an extent-mapped inode on a volume without
    /// the EXTENTS feature corrupt, and the new directory and slow symlink
    /// were extent-mapped whatever the volume (#89).
    fn map_one_block(&self, raw: &mut [u8], phys: u64) -> Result<()> {
        use crate::inode::{OFF_BLOCK, OFF_FLAGS};
        if !self.flavor.uses_extents() {
            let direct =
                u32::try_from(phys).map_err(|_| Error::Corrupt("block past a 32-bit block map"))?;
            raw[OFF_BLOCK..OFF_BLOCK + 4].copy_from_slice(&direct.to_le_bytes());
            return Ok(());
        }
        let flags = u32::from_le_bytes(raw[OFF_FLAGS..OFF_FLAGS + 4].try_into().unwrap());
        raw[OFF_FLAGS..OFF_FLAGS + 4]
            .copy_from_slice(&(flags | crate::inode::InodeFlags::EXTENTS.bits()).to_le_bytes());

        // i_block: extent leaf header (1 entry, max 4, depth 0) + one extent,
        // logical block 0, length 1.
        let header = OFF_BLOCK;
        raw[header..header + 2].copy_from_slice(&crate::extent::EXT4_EXT_MAGIC.to_le_bytes());
        raw[header + 2..header + 4].copy_from_slice(&1u16.to_le_bytes());
        raw[header + 4..header + 6].copy_from_slice(&4u16.to_le_bytes());
        raw[header + 6..header + 8].copy_from_slice(&0u16.to_le_bytes());
        let entry = header + 12;
        raw[entry..entry + 4].copy_from_slice(&0u32.to_le_bytes());
        raw[entry + 4..entry + 6].copy_from_slice(&1u16.to_le_bytes());
        let (phys_hi, phys_lo) = crate::extent_mut::split_phys_block(phys);
        raw[entry + 6..entry + 8].copy_from_slice(&phys_hi.to_le_bytes());
        raw[entry + 8..entry + 12].copy_from_slice(&phys_lo.to_le_bytes());
        Ok(())
    }

    /// Compose a slow-symlink inode image: `S_IFLNK | 0o777`, 1 link,
    /// `i_size = target.len()`, logical block 0 mapped to `data_phys` by
    /// [`Self::map_one_block`]. One fs
    /// block worth of 512-byte sectors charged to `i_blocks`.
    ///
    /// Caller must have already written the target bytes (zero-padded) to
    /// `data_phys * block_size`.
    fn build_slow_symlink_inode(&self, ino: u32, target: &[u8], data_phys: u64) -> Result<Vec<u8>> {
        use crate::inode::{OFF_BLOCKS_LO, OFF_LINKS_COUNT, OFF_MODE, OFF_SIZE_LO};
        debug_assert!(target.len() >= 60 && target.len() <= 4096);
        let mut raw = vec![0u8; self.sb.inode_size as usize];

        let mode_bits = crate::inode::S_IFLNK | 0o0777;
        raw[OFF_MODE..OFF_MODE + 2].copy_from_slice(&mode_bits.to_le_bytes());
        raw[OFF_SIZE_LO..OFF_SIZE_LO + 4].copy_from_slice(&(target.len() as u32).to_le_bytes());
        raw[OFF_LINKS_COUNT..OFF_LINKS_COUNT + 2].copy_from_slice(&1u16.to_le_bytes());
        let bs = self.sb.block_size() as u64;
        let sectors = bs / 512;
        raw[OFF_BLOCKS_LO..OFF_BLOCKS_LO + 4].copy_from_slice(&(sectors as u32).to_le_bytes());
        self.map_one_block(&mut raw, data_phys)?;

        // i_extra_isize first: it says whether the epoch bits have a home.
        write_inode_extra_isize(&mut raw);
        let now = self.runtime.now_unix_seconds();
        write_inode_timestamps(&mut raw, now);
        let generation = self.runtime.next_inode_generation();
        write_inode_generation(&mut raw, generation);
        self.stamp_inode_checksum(&mut raw, ino, generation);
        Ok(raw)
    }

    /// Compose a fresh regular-file inode image: `S_IFREG | mode`, 1 link,
    /// 0 size, 0 blocks, EXTENTS flag set with an empty 4-entry leaf root,
    /// timestamps = now, generation = process-id-derived counter, extra_isize
    /// = 32 so the inode has room for nsec timestamps + checksum_hi.
    fn build_regular_file_inode(&self, ino: u32, mode: u16) -> Result<Vec<u8>> {
        use crate::inode::{OFF_BLOCK, OFF_FLAGS, OFF_LINKS_COUNT, OFF_MODE};
        let mut raw = vec![0u8; self.sb.inode_size as usize];

        let mode_bits = crate::inode::S_IFREG | (mode & 0x0FFF);
        raw[OFF_MODE..OFF_MODE + 2].copy_from_slice(&mode_bits.to_le_bytes());
        raw[OFF_LINKS_COUNT..OFF_LINKS_COUNT + 2].copy_from_slice(&1u16.to_le_bytes());

        // i_flags + i_block layout depend on the FS dialect:
        // - ext4 (FsFlavor::Ext4): EXTENTS_FL set, i_block holds an empty
        //   extent leaf header (magic + entries=0 + max=4 + depth=0).
        // - ext2 / ext3: no flag, i_block stays all-zero (no direct or
        //   indirect pointers — file is empty so there's nothing to map).
        if self.flavor.uses_extents() {
            raw[OFF_FLAGS..OFF_FLAGS + 4]
                .copy_from_slice(&crate::inode::InodeFlags::EXTENTS.bits().to_le_bytes());

            let extent_header_off = OFF_BLOCK;
            raw[extent_header_off..extent_header_off + 2]
                .copy_from_slice(&crate::extent::EXT4_EXT_MAGIC.to_le_bytes());
            raw[extent_header_off + 2..extent_header_off + 4].copy_from_slice(&0u16.to_le_bytes());
            raw[extent_header_off + 4..extent_header_off + 6].copy_from_slice(&4u16.to_le_bytes());
            raw[extent_header_off + 6..extent_header_off + 8].copy_from_slice(&0u16.to_le_bytes());
        }

        // i_extra_isize first: it says whether the epoch bits have a home.
        write_inode_extra_isize(&mut raw);
        let now = self.runtime.now_unix_seconds();
        write_inode_timestamps(&mut raw, now);
        let generation = self.runtime.next_inode_generation();
        write_inode_generation(&mut raw, generation);
        self.stamp_inode_checksum(&mut raw, ino, generation);
        Ok(raw)
    }

    /// Replace the content of `path` with `data`. The file must already
    /// exist. Frees every existing extent, allocates a single contiguous run
    /// of blocks large enough for `data`, writes the bytes (zero-padding the
    /// tail of the last block), then inserts one extent into the inode.
    ///
    /// This is the "Finder just saved a document" path — complete rewrite of
    /// a file. Piecewise writes / appends / sparse writes come later.
    ///
    /// For an extent-mapped inode on a mount with a journal, atomic across
    /// the whole replace: freeing the old data, allocating the new run, the
    /// bitmap, BGD and superblock updates, the new block contents and the
    /// inode all commit as one transaction. Two cases are not (#179):
    ///
    /// - without a journal, the same blocks are written in turn, and a
    ///   crash part-way leaves some written and some not;
    /// - an inode that is not extent-mapped goes to
    ///   `apply_replace_file_content_indirect`, which builds no transaction
    ///   at all, on ext3 with a journal too.
    ///
    /// Returns the new file size on success.
    pub fn apply_replace_file_content(&self, path: &str, data: &[u8]) -> Result<u64> {
        self.apply_replace_file_content_bytes(path.as_bytes(), data)
    }

    /// [`apply_replace_file_content`](Self::apply_replace_file_content) of a path given as bytes, never decoded.
    pub(crate) fn apply_replace_file_content_bytes(&self, path: &[u8], data: &[u8]) -> Result<u64> {
        self.refuse_write()?;
        let ino = self.resolve(path)?;
        let (inode, mut raw) = self.read_inode_verified(ino)?;
        if !inode.is_file() {
            return Err(Error::InvalidArgument(
                "write_file target is not a regular file",
            ));
        }
        Self::refuse_inline_data_write(&inode)?;
        if !inode.has_extents() {
            // ext2 / ext3 (or ext4 inode without EXTENTS_FL): legacy
            // direct/indirect block-pointer scheme. Same overall shape as
            // the extent path below — free old → allocate → write data →
            // patch inode — but the i_block tree comes from `indirect_mut`
            // and any indirect-tree blocks are co-allocated with the data
            // run (one bitmap call covers both).
            return self.apply_replace_file_content_indirect(ino, inode, raw, data);
        }

        let bs = self.sb.block_size();
        let sectors_per_block = bs as u64 / 512;
        let group_idx_of_inode = ((ino - 1) / self.sb.inodes_per_group) as usize;

        // Multi-block transaction: free existing data + alloc new run +
        // bitmap + BGD + SB + new data block contents + inode update.
        // Atomic across the whole replace.
        let mut buf = BlockBuffer::new(bs);

        // Phase 1: free existing data blocks. Each freed run credits its
        // own group's BGD via `buffer_free_block_run_and_bgd`.
        let runs = self.extent_tree_runs(ino, &inode)?;
        let freed_fs_blocks = self.buffer_free_runs(&mut buf, &runs)?;

        // Reset the inode's extent root to an empty leaf.
        let mut root = vec![0u8; 60];
        root[0..2].copy_from_slice(&crate::extent::EXT4_EXT_MAGIC.to_le_bytes());
        root[4..6].copy_from_slice(&4u16.to_le_bytes()); // max entries
        Self::patch_inode_block_area(&mut raw, &root)?;

        // Empty write: BGDs already credited per-run above; only SB needs
        // a single update + inode rewrite.
        if data.is_empty() {
            self.finalize_inode_raw_after_write(
                ino,
                &mut raw,
                &inode,
                0,
                Self::xattr_block_sectors(&inode, bs),
            )?;
            if freed_fs_blocks > 0 {
                self.buffer_patch_sb_counters(&mut buf, freed_fs_blocks as i64, 0)?;
            }
            self.buffer_write_inode(&mut buf, ino, &raw)?;
            self.commit_block_buffer(buf)?;
            return Ok(0);
        }

        // Phase 2: allocate one contiguous run for the whole payload.
        let needed_blocks: u32 = data.len().div_ceil(bs as usize) as u32;
        let mut bitmap_reader = |block: u64| self.read_block(block);
        let plan = crate::alloc::plan_block_allocation(
            &self.sb,
            &self.allocation_groups(),
            needed_blocks,
            group_idx_of_inode as u32,
            &mut bitmap_reader,
        )?;

        // Phase 3: mark allocated bitmap + patch destination BGD; SB nets
        // the alloc delta against the freed total computed above.
        self.buffer_mark_block_run_used(&mut buf, plan.first_block, needed_blocks as u64)?;
        self.buffer_patch_bgd_counters(
            &mut buf,
            plan.bgd.group_idx as usize,
            plan.bgd.free_blocks_delta,
            plan.bgd.free_inodes_delta,
            plan.bgd.used_dirs_delta,
        )?;
        let net_block_delta = freed_fs_blocks as i64 - needed_blocks as i64;
        self.buffer_patch_sb_counters(&mut buf, net_block_delta, 0)?;

        // Phase 4: stage the payload into the allocated physical run.
        for i in 0..needed_blocks as u64 {
            let off_in_data = (i as usize) * bs as usize;
            let chunk_end = ((i as usize + 1) * bs as usize).min(data.len());
            let mut block = vec![0u8; bs as usize];
            block[..chunk_end - off_in_data].copy_from_slice(&data[off_in_data..chunk_end]);
            buf.put(plan.first_block + i, block);
        }

        // Phase 5: insert the single extent into the (now-empty) inline
        // root and stage the inode.
        let new_extent = crate::extent::Extent {
            logical_block: 0,
            length: needed_blocks as u16,
            physical_block: plan.first_block,
            uninitialized: false,
        };
        let muts = crate::extent_mut::plan_insert_extent(&root, new_extent)?;
        for m in &muts {
            if let crate::extent_mut::ExtentMutation::WriteRoot { bytes } = m {
                Self::patch_inode_block_area(&mut raw, bytes)?;
            }
        }
        let new_size = data.len() as u64;
        let new_sectors =
            needed_blocks as u64 * sectors_per_block + Self::xattr_block_sectors(&inode, bs);
        self.finalize_inode_raw_after_write(ino, &mut raw, &inode, new_size, new_sectors)?;
        self.buffer_write_inode(&mut buf, ino, &raw)?;

        self.commit_block_buffer(buf)?;
        Ok(new_size)
    }

    /// ext2/ext3 sibling of `apply_replace_file_content`'s extent path.
    /// Frees the inode's existing direct/indirect tree, allocates one
    /// contiguous run sized for both the data payload AND the indirect-tree
    /// metadata blocks, builds the new tree via `indirect_mut::plan_contiguous`,
    /// then persists everything (data → indirect blocks → inode).
    ///
    /// No journal interaction, on any mount. The writer can address an
    /// indirect-mapped journal inode (see `mount_inner`), so an ext3 mount
    /// has one, but this path frees, allocates and writes block by block
    /// and never builds a `BlockBuffer` for it. A crash after the old runs
    /// are freed and before the inode is rewritten leaves the inode
    /// mapping blocks the bitmap already calls free (#179).
    /// Whether a block-mapped inode's `i_block` holds block pointers.
    ///
    /// Not every inode's does: a fast symlink keeps its target there and a
    /// device node its numbers, with no data blocks at all. `i_blocks`
    /// tells them apart, less the external xattr block it also counts.
    fn holds_block_map(inode: &Inode, block_size: u32) -> bool {
        let xattr_sectors = if inode.file_acl != 0 {
            u64::from(block_size) / 512
        } else {
            0
        };
        inode.flags & crate::inode::InodeFlags::INLINE_DATA.bits() == 0
            && inode.blocks > xattr_sectors
    }

    /// Free every data and indirect block a block-mapped inode holds,
    /// staged in `buf`. Returns the blocks freed. Does nothing for an inode
    /// whose `i_block` holds no pointers ([`Self::holds_block_map`]).
    fn buffer_free_block_map(&self, buf: &mut BlockBuffer, inode: &Inode) -> Result<u64> {
        let bs = self.sb.block_size();
        if !Self::holds_block_map(inode, bs) {
            return Ok(0);
        }
        let freed = crate::indirect_mut::collect_for_free(
            &inode.block,
            bs,
            inode.size.div_ceil(u64::from(bs)) as u32,
            self.dev.as_ref(),
        )?;
        let mut count = 0;
        for run in &freed.data_runs {
            count += self.buffer_free_block_run_and_bgd(buf, run.start, run.len as u64)?;
        }
        for &iblk in &freed.indirect_blocks {
            count += self.buffer_free_block_run_and_bgd(buf, iblk, 1)?;
        }
        Ok(count)
    }

    /// The sectors an inode's external xattr block adds to `i_blocks`.
    ///
    /// `i_blocks` counts every block the inode holds, and the xattr block is
    /// one of them. Replacing a file's content replaces its data, not its
    /// attributes, so the count set afterwards keeps this.
    fn xattr_block_sectors(inode: &Inode, block_size: u32) -> u64 {
        if inode.file_acl != 0 {
            u64::from(block_size) / 512
        } else {
            0
        }
    }

    fn apply_replace_file_content_indirect(
        &self,
        ino: u32,
        inode: Inode,
        mut raw: Vec<u8>,
        data: &[u8],
    ) -> Result<u64> {
        let bs = self.sb.block_size();
        let sectors_per_block = bs as u64 / 512;
        let group_idx_of_inode = ((ino - 1) / self.sb.inodes_per_group) as usize;

        // Phase 1: free existing data + indirect-tree blocks. `collect_for_free`
        // walks the tree and returns coalesced data runs + individual indirect
        // blocks, so cross-group fragmented files are accounted for correctly.
        // THROUGH THE BUFFER, as the extent path goes. The unbuffered
        // helpers wrote the block bitmap without restamping its checksum,
        // and outside the journal.
        let mut buf = BlockBuffer::new(bs);
        let mut freed_fs_blocks: u64 = 0;
        if inode.size > 0 {
            let block_count = inode.size.div_ceil(bs as u64) as u32;
            let freed = crate::indirect_mut::collect_for_free(
                &inode.block,
                bs,
                block_count,
                self.dev.as_ref(),
            )?;
            for run in &freed.data_runs {
                freed_fs_blocks +=
                    self.buffer_free_block_run_and_bgd(&mut buf, run.start, run.len as u64)?;
            }
            for &iblk in &freed.indirect_blocks {
                freed_fs_blocks += self.buffer_free_block_run_and_bgd(&mut buf, iblk, 1)?;
            }
        }
        // Reset i_block to all zeros — no extent magic for legacy inodes.
        let zero_iblock = [0u8; 60];
        Self::patch_inode_block_area(&mut raw, &zero_iblock)?;

        if data.is_empty() {
            self.finalize_inode_raw_after_write(
                ino,
                &mut raw,
                &inode,
                0,
                Self::xattr_block_sectors(&inode, bs),
            )?;
            if freed_fs_blocks > 0 {
                self.buffer_patch_sb_counters(&mut buf, freed_fs_blocks as i64, 0)?;
            }
            self.buffer_write_inode(&mut buf, ino, &raw)?;
            self.commit_block_buffer(buf)?;
            return Ok(0);
        }

        // Phase 2: allocate one contiguous run sized for data + indirect tree.
        // Indirect blocks live at the head of the run, data at the tail.
        // `count_indirect_blocks` is exactly the number of allocator pulls
        // `plan_contiguous` will make, so the budget is tight (verified by
        // the `count_indirect_blocks_matches_plan_contiguous` unit test).
        let needed_data_blocks: u32 = data.len().div_ceil(bs as usize) as u32;
        let n_indirect: u32 = crate::indirect_mut::count_indirect_blocks(needed_data_blocks, bs)
            .try_into()
            .map_err(|_| Error::Corrupt("indirect_mut: indirect block count overflow"))?;
        let total_run = needed_data_blocks
            .checked_add(n_indirect)
            .ok_or(Error::Corrupt("indirect_mut: total run count overflow"))?;

        let mut bitmap_reader = |block: u64| self.read_block(block);
        let plan = crate::alloc::plan_block_allocation(
            &self.sb,
            &self.allocation_groups(),
            total_run,
            group_idx_of_inode as u32,
            &mut bitmap_reader,
        )?;
        let first_indirect = plan.first_block;
        let first_data = plan.first_block + n_indirect as u64;

        // Phase 3: build the indirect tree. The closure hands out blocks
        // sequentially from `first_indirect` — `plan_contiguous` doesn't
        // care about address ordering, so any allocation order is fine.
        let mut next_indirect = first_indirect;
        let i_plan =
            crate::indirect_mut::plan_contiguous(needed_data_blocks, first_data, bs, || {
                let v = next_indirect;
                next_indirect += 1;
                Ok(v)
            })?;

        // Phase 4: bitmap + BGD + SB counters cover the whole run in one
        // mark-used + one BGD-credit + one SB-update.
        self.buffer_mark_block_run_used(&mut buf, plan.first_block, total_run as u64)?;
        self.buffer_patch_bgd_counters(
            &mut buf,
            plan.bgd.group_idx as usize,
            plan.bgd.free_blocks_delta,
            plan.bgd.free_inodes_delta,
            plan.bgd.used_dirs_delta,
        )?;
        let net_block_delta = freed_fs_blocks as i64 - total_run as i64;
        self.buffer_patch_sb_counters(&mut buf, net_block_delta, 0)?;

        // Phase 5: write the data payload into the data-portion of the run.
        for i in 0..needed_data_blocks as u64 {
            let off_in_data = (i as usize) * bs as usize;
            let chunk_end = ((i as usize + 1) * bs as usize).min(data.len());
            let mut block = vec![0u8; bs as usize];
            block[..chunk_end - off_in_data].copy_from_slice(&data[off_in_data..chunk_end]);
            buf.put(first_data + i, block);
        }

        // Phase 6: write the indirect-tree blocks.
        for (blk, bytes) in &i_plan.block_writes {
            buf.put(*blk, bytes.clone());
        }

        // Phase 7: patch i_block region with the new tree root.
        Self::patch_inode_block_area(&mut raw, &i_plan.i_block)?;

        // Phase 8: finalize. ext2/3 i_blocks counts BOTH data AND indirect
        // blocks (in 512-byte sectors) — extent metadata blocks count the
        // same way for ext4 so the rule is consistent across flavors.
        let new_size = data.len() as u64;
        let new_sectors = (needed_data_blocks as u64 + n_indirect as u64) * sectors_per_block
            + Self::xattr_block_sectors(&inode, bs);
        self.finalize_inode_raw_after_write(ino, &mut raw, &inode, new_size, new_sectors)?;
        self.buffer_write_inode(&mut buf, ino, &raw)?;
        self.commit_block_buffer(buf)?;
        Ok(new_size)
    }

    /// The largest write, in data blocks, that `apply_pwrite` puts in one
    /// transaction.
    ///
    /// Two bounds, the smaller winning. MEMORY: a chunk is held several
    /// times over while it is committed (the block buffer, the transaction,
    /// the serialised journal blocks), so no chunk exceeds
    /// [`PWRITE_CHUNK_MAX_BYTES`]. THE JOURNAL: the chunk's data blocks,
    /// the metadata it dirties, the descriptor blocks tagging all of them
    /// and the commit block must fit in the journal's usable length.
    ///
    /// The metadata is bounded for the worst case, free space fragmented
    /// into single blocks: every data block its own allocation, so every
    /// block group touched has its bitmap and descriptor block dirtied, and
    /// every block its own extent, so the extent tree grows by a leaf per
    /// half-leaf of blocks and by index nodes above those. Descriptor blocks
    /// are counted at the widest tag with a checksum tail, the fewest tags a
    /// descriptor can hold.
    fn pwrite_chunk_blocks(&self) -> u64 {
        let bs = self.sb.block_size() as u64;
        let memory_bound = (PWRITE_CHUNK_MAX_BYTES / bs).max(1);
        let Some(journal) = &self.journal else {
            return memory_bound;
        };
        let capacity = match journal.lock() {
            Ok(jw) => jw.max_blocks_per_transaction() as u64,
            // The commit refuses a poisoned writer anyway; any chunk will do.
            Err(_) => return memory_bound,
        };
        // Widest JBD2 tag (16 bytes, CSUM_V3) and a 4-byte checksum tail
        // after the 12-byte header: the fewest tags a descriptor holds.
        let tags_per_desc = ((bs - 12 - 4) / 16).max(1);
        // Extents per tree node (12-byte entries after a 12-byte header),
        // halved: a split leaves both halves this full.
        let half_node = (((bs - 12) / 12) / 2).max(1);
        let groups = self.groups.len() as u64;
        let gdt_blocks = (groups * u64::from(self.sb.desc_size)).div_ceil(bs);
        let fits = |data: u64| {
            // Extent-tree blocks: leaves for `data` single-block extents, and
            // index levels above them, up to the format's depth limit of 5.
            let mut tree = 0u64;
            let mut level = data;
            for _ in 0..5 {
                level = level.div_ceil(half_node) + 1;
                tree += level;
            }
            let allocations = data + tree;
            // Inode table block + superblock, then a bitmap per group and the
            // descriptor-table blocks covering them.
            let metadata = 2 + allocations.min(groups) + allocations.min(gdt_blocks) + tree;
            let tagged = data + metadata;
            let descriptors = tagged.div_ceil(tags_per_desc);
            let commit_block = 1;
            tagged + descriptors + commit_block <= capacity
        };
        // The largest `data` that fits, by bisection (`fits` is monotone).
        let (mut lo, mut hi) = (1u64, memory_bound);
        if fits(hi) {
            return hi;
        }
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if fits(mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Positional write: splice `data` into the file at byte `offset`,
    /// allocating new physical blocks for any logical blocks that aren't
    /// yet mapped (sparse holes, or blocks past EOF). Existing mapped
    /// blocks are read-modify-written for partial overlap; full-block
    /// writes go in fresh.
    ///
    /// This is the primitive needed by streaming write paths
    /// (FUSE/WinFsp/FSKit cache-manager dispatches) — `apply_replace_file_content`
    /// is "save-as", `apply_pwrite` is `pwrite(2)`.
    ///
    /// Returns the new file size on success.
    ///
    /// Allocation behaviour:
    /// - Each unmapped logical run is satisfied by one or more physical
    ///   runs. If `plan_block_allocation` can't find a single contiguous
    ///   group-local run sized for the whole logical run, the request is
    ///   halved and retried — each successful sub-run becomes its own
    ///   extent. True ENOSPC (single-block allocation also fails)
    ///   surfaces as `Error::NoSpaceLeftOnDevice`.
    /// - Extent inserts try the inline-root path first; on
    ///   `LEAF_FULL_NEEDS_PROMOTION` they fall back to
    ///   `plan_insert_extent_deep`, which promotes the tree to depth ≥ 1
    ///   and allocates the additional internal/leaf node blocks via the
    ///   same buffer-aware allocator. Tail checksums on tree blocks are
    ///   patched when `metadata_csum` is on.
    ///
    /// v1 limitations:
    /// - Extent-tree inodes only. Legacy ext2/3 (direct/indirect blocks)
    ///   returns `Error::InvalidArgument`. The streaming-copy use case
    ///   for this path is on freshly-mkfs'd ext4 volumes that always
    ///   have `EXTENTS_FL`.
    /// - Pre-existing uninitialised extents (from `fallocate`) in the
    ///   write range: not handled — the unmapped-run walk treats them
    ///   the same as holes and tries to insert a fresh extent that
    ///   would overlap, hitting `CorruptExtentTree("extent overlaps
    ///   existing")`. Skipping fallocate-then-write, the streaming
    ///   copy path doesn't trigger this.
    pub fn apply_pwrite(&self, path: &str, offset: u64, data: &[u8]) -> Result<u64> {
        self.apply_pwrite_bytes(path.as_bytes(), offset, data)
    }

    /// [`apply_pwrite`](Self::apply_pwrite) of a path given as bytes, never decoded.
    pub(crate) fn apply_pwrite_bytes(&self, path: &[u8], offset: u64, data: &[u8]) -> Result<u64> {
        self.refuse_write()?;
        let ino = self.resolve(path)?;
        self.apply_pwrite_ino(ino, offset, data)
    }

    /// [`apply_pwrite`](Self::apply_pwrite) on the inode `r` names.
    pub fn apply_pwrite_ino(
        &self,
        r: impl Into<InodeRef>,
        offset: u64,
        data: &[u8],
    ) -> Result<u64> {
        self.refuse_write()?;
        let r = r.into();
        let ino = r.ino;
        let (inode, mut raw) = self.live_inode(r)?;
        if !inode.is_file() {
            return Err(Error::InvalidArgument(
                "pwrite target is not a regular file",
            ));
        }
        Self::refuse_inline_data_write(&inode)?;
        if !inode.has_extents() {
            return Err(Error::InvalidArgument(
                "pwrite: legacy (non-extents) inodes not supported in v1",
            ));
        }

        if data.is_empty() {
            // No-op (no size change either — a zero-length pwrite at any
            // offset is a no-op per POSIX `pwrite(2)`).
            return Ok(inode.size);
        }

        let bs = self.sb.block_size() as u64;
        let bs_usize = bs as usize;
        let sectors_per_block = bs / 512;
        let len = data.len() as u64;
        let end = offset
            .checked_add(len)
            .ok_or(Error::InvalidArgument("pwrite: offset+len overflow"))?;
        let first_lb = offset / bs;
        let last_lb_excl = end.div_ceil(bs);

        // A pwrite journals its data blocks and its metadata in ONE
        // transaction, and a transaction has to fit in the journal: the
        // writer checkpoints every commit, so the whole log is free for each
        // one, and it refuses a transaction longer than that. A write larger
        // than the chunk the journal can take is cut into block-aligned
        // chunks, each its own transaction and each atomic (POSIX pwrite is
        // not atomic across a large range anyway). A transaction carries as
        // many descriptor blocks as it needs (#147), so descriptor capacity
        // is not the bound; it only counts toward the journal's length (#293).
        let max_data_blocks = self.pwrite_chunk_blocks();
        let max_chunk = max_data_blocks * bs;
        if len > max_chunk {
            let mut chunk_off = 0u64;
            while chunk_off < len {
                let take = max_chunk.min(len - chunk_off);
                let s = chunk_off as usize;
                let e = (chunk_off + take) as usize;
                self.apply_pwrite_ino(r, offset + chunk_off, &data[s..e])?;
                chunk_off += take;
            }
            let (after, _) = self.read_inode_verified(ino)?;
            return Ok(after.size);
        }

        // Working copy of the 60-byte inline extent root. Updated in place
        // as we insert extents for each unmapped run; patched into `raw`
        // once at the end.
        let mut root_bytes: Vec<u8> = inode.block.to_vec();
        // The tree nodes this call has planned, staged here and in `buf`
        // until the commit; every lookup below reads through them (#389).
        let mut tree_nodes: std::collections::BTreeMap<u64, Vec<u8>> =
            std::collections::BTreeMap::new();

        let mut buf = BlockBuffer::new(self.sb.block_size());
        let group_idx_of_inode = (ino - 1) / self.sb.inodes_per_group;

        // Track which logical blocks were freshly allocated by this call.
        // Phase-2 writes for these MUST NOT read from disk (the prior
        // contents of those physical blocks are stale junk from whoever
        // freed them last); they get a zero-init buffer instead.
        let mut newly_alloc: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        let mut alloc_total_blocks: u64 = 0;

        // A PREALLOCATED BLOCK IS NOT A HOLE. `map_logical` answers `None`
        // for an uninitialized extent, so reads see zeros, and Phase 1
        // below took that for a hole: it allocated a fresh block and was
        // refused inserting an extent over the preallocated one. The
        // blocks already belong to the file, so the write goes into them
        // and the range it covers becomes initialized. They count as new
        // in Phase 2: a zero-filled block holds the data, so whatever the
        // preallocated block held never becomes readable.
        let mut preallocated = false;
        for lb in first_lb..last_lb_excl.min(u64::from(u32::MAX)) {
            if let Some(e) = crate::extent::lookup(
                &root_bytes,
                &StagedTreeNodes {
                    fs: self,
                    nodes: &tree_nodes,
                },
                self.sb.block_size(),
                lb,
            )? {
                if e.uninitialized {
                    preallocated = true;
                    break;
                }
            }
        }
        if preallocated {
            let (new_root, converted) = crate::extent_mut::plan_initialize_range(
                &root_bytes,
                first_lb as u32,
                last_lb_excl.min(u64::from(u32::MAX)) as u32,
            )
            .map_err(|e| match e {
                Error::CorruptExtentTree(msg)
                    if msg.contains("multi-level") || msg.contains("LEAF_FULL") =>
                {
                    Error::Unsupported(
                        "pwrite: writing into a preallocated range needs its extent split \
                         in a tree deeper than the inode's inline root",
                    )
                }
                e => e,
            })?;
            root_bytes = new_root;
            newly_alloc.extend(converted.into_iter().map(u64::from));
        }

        // Phase 1: walk affected logical blocks; allocate each contiguous
        // unmapped run as one physical extent and stage the bitmap/BGD
        // updates. Repeated `map_logical` calls re-parse `root_bytes` each
        // time, so the in-progress inserts are visible to subsequent
        // lookups in the same loop.
        let mut lb = first_lb;
        while lb < last_lb_excl {
            let mapped = crate::extent::map_logical(
                &root_bytes,
                &StagedTreeNodes {
                    fs: self,
                    nodes: &tree_nodes,
                },
                self.sb.block_size(),
                lb,
            )?;
            if mapped.is_some() {
                lb += 1;
                continue;
            }
            // Find the end of this unmapped run.
            let mut run_end = lb + 1;
            while run_end < last_lb_excl {
                let p = crate::extent::map_logical(
                    &root_bytes,
                    &StagedTreeNodes {
                        fs: self,
                        nodes: &tree_nodes,
                    },
                    self.sb.block_size(),
                    run_end,
                )?;
                if p.is_some() {
                    break;
                }
                run_end += 1;
            }
            let run_len_u64 = run_end - lb;
            if run_len_u64 > u32::MAX as u64 {
                return Err(Error::InvalidArgument(
                    "pwrite: unmapped run exceeds u32 block count",
                ));
            }

            // Allocate physical blocks for this logical run, splitting
            // across smaller contiguous physical runs when no single
            // group has a free run that size. Each sub-allocation is
            // staged into the buffer (bitmap + BGD) and inserted as its
            // own extent. plan_insert_extent auto-merges adjacent extents
            // so the *common* sequential-write case still produces one
            // extent overall.
            let mut remaining_in_run = run_len_u64 as u32;
            let mut sub_lb = lb;
            while remaining_in_run > 0 {
                let mut want = remaining_in_run;
                let plan = loop {
                    let plan_result =
                        self.plan_buffered_block_allocation(&buf, want, group_idx_of_inode);
                    match plan_result {
                        Ok(p) => break p,
                        Err(Error::Corrupt(msg)) if msg.contains("contiguous free run") => {
                            if want == 1 {
                                // Even a single block isn't available
                                // anywhere — true ENOSPC.
                                return Err(Error::NoSpaceLeftOnDevice);
                            }
                            // Fragmented: halve the request and retry.
                            // Each successful sub-run becomes its own
                            // extent; the outer while loop keeps drawing
                            // until the whole logical run is covered.
                            want /= 2;
                        }
                        Err(e) => return Err(e),
                    }
                };

                let got = want;
                let got_u64 = got as u64;

                self.buffer_mark_block_run_used(&mut buf, plan.first_block, got_u64)?;
                self.buffer_patch_bgd_counters(
                    &mut buf,
                    plan.bgd.group_idx as usize,
                    plan.bgd.free_blocks_delta,
                    plan.bgd.free_inodes_delta,
                    plan.bgd.used_dirs_delta,
                )?;
                alloc_total_blocks += got_u64;

                let new_extent = crate::extent::Extent {
                    logical_block: sub_lb as u32,
                    length: got as u16,
                    physical_block: plan.first_block,
                    uninitialized: false,
                };

                // Try the inline-root insert first; on overflow fall back
                // to the depth-promoting deep insert. Both paths produce a
                // new 60-byte root that we splice into `raw` at the end.
                match crate::extent_mut::plan_insert_extent(&root_bytes, new_extent) {
                    Ok(muts) => {
                        for m in &muts {
                            if let crate::extent_mut::ExtentMutation::WriteRoot { bytes } = m {
                                root_bytes = bytes.clone();
                            }
                        }
                    }
                    Err(Error::CorruptExtentTree(msg))
                        if msg.contains("LEAF_FULL_NEEDS_PROMOTION")
                            || msg.contains("multi-level tree mutation") =>
                    {
                        // Two distinct failures both route to the deep path:
                        // 1. Inline leaf root has 4 entries already
                        //    (LEAF_FULL_NEEDS_PROMOTION) → promote to depth 1.
                        // 2. Root has *already* been promoted on a prior
                        //    insert in this same call → root is an index
                        //    node, so the inline-leaf-only `plan_insert_extent`
                        //    bails with "multi-level tree mutation". The
                        //    deep planner descends correctly.
                        // Allocate tree-meta blocks one at a time via the
                        // same buffer-aware allocator. Each call stages a
                        // bitmap + BGD update so subsequent allocations
                        // see the just-claimed bits.
                        let reader = StagedTreeNodes {
                            fs: self,
                            nodes: &tree_nodes,
                        };
                        let mut meta_blocks_alloc: u64 = 0;
                        let inode_generation = inode.generation;
                        let deep_plan = {
                            let mut alloc_closure = || -> Result<u64> {
                                let p = self.plan_buffered_block_allocation(
                                    &buf,
                                    1,
                                    group_idx_of_inode,
                                )?;
                                self.buffer_mark_block_run_used(&mut buf, p.first_block, 1)?;
                                self.buffer_patch_bgd_counters(
                                    &mut buf,
                                    p.bgd.group_idx as usize,
                                    p.bgd.free_blocks_delta,
                                    0,
                                    0,
                                )?;
                                meta_blocks_alloc += 1;
                                Ok(p.first_block)
                            };
                            crate::extent_mut::plan_insert_extent_deep(
                                &root_bytes,
                                new_extent,
                                self.sb.block_size(),
                                &reader,
                                &mut alloc_closure,
                            )?
                        };
                        root_bytes = deep_plan.new_root;
                        for (block, bytes) in deep_plan.block_writes {
                            let mut bytes = bytes;
                            if self.csum.enabled {
                                self.csum
                                    .patch_extent_tail(ino, inode_generation, &mut bytes);
                            }
                            // Staged, never written ahead of the commit: a
                            // later sub-run's plan and every lookup read it
                            // back through `tree_nodes` (#389).
                            tree_nodes.insert(block, bytes.clone());
                            buf.put(block, bytes);
                        }
                        alloc_total_blocks += meta_blocks_alloc;
                    }
                    Err(e) => return Err(e),
                }

                // Mark these logical blocks as freshly-allocated so Phase 2
                // writes use put() (zero-init) instead of get_mut()
                // (read-from-disk-and-modify).
                for x in sub_lb..(sub_lb + got_u64) {
                    newly_alloc.insert(x);
                }

                sub_lb += got_u64;
                remaining_in_run -= got;
            }

            lb = run_end;
        }

        // Phase 2: splice the chunk into each affected block.
        let mut data_off: usize = 0;
        for cur_lb in first_lb..last_lb_excl {
            let block_byte_start = cur_lb * bs;
            let block_byte_end = block_byte_start + bs;
            let chunk_start = offset.max(block_byte_start);
            let chunk_end = end.min(block_byte_end);
            let in_block_off = (chunk_start - block_byte_start) as usize;
            let chunk_len = (chunk_end - chunk_start) as usize;

            let phys = crate::extent::map_logical(
                &root_bytes,
                &StagedTreeNodes { fs: self, nodes: &tree_nodes },
                self.sb.block_size(),
                cur_lb,
            )?
            .ok_or(Error::Corrupt(
                "pwrite Phase 2: logical block unmapped after Phase 1 (allocator/extent insert mismatch)",
            ))?;

            if newly_alloc.contains(&cur_lb) {
                // Fresh block: zero-init then splice. Avoids reading stale
                // bytes from a previously-freed extent.
                let mut block = vec![0u8; bs_usize];
                block[in_block_off..in_block_off + chunk_len]
                    .copy_from_slice(&data[data_off..data_off + chunk_len]);
                buf.put(phys, block);
            } else {
                // Existing block: read-modify-write to preserve untouched
                // bytes (head before `chunk_start`, tail after `chunk_end`).
                let block = buf.get_mut(self, phys)?;
                if block.len() != bs_usize {
                    return Err(Error::Corrupt(
                        "pwrite Phase 2: existing block has wrong size",
                    ));
                }
                block[in_block_off..in_block_off + chunk_len]
                    .copy_from_slice(&data[data_off..data_off + chunk_len]);
            }

            data_off += chunk_len;
        }
        debug_assert_eq!(data_off, data.len());

        // Phase 3: patch the extent root onto `raw`, update size + sectors,
        // recompute the inode checksum, stage the inode write.
        Self::patch_inode_block_area(&mut raw, &root_bytes)?;
        let new_size = inode.size.max(end);
        let new_sectors = inode
            .blocks
            .checked_add(alloc_total_blocks * sectors_per_block)
            .ok_or(Error::Corrupt("pwrite: i_blocks overflow"))?;
        self.finalize_inode_raw_after_write(ino, &mut raw, &inode, new_size, new_sectors)?;
        self.buffer_write_inode(&mut buf, ino, &raw)?;

        // Phase 4: SB delta for the newly-allocated blocks.
        if alloc_total_blocks > 0 {
            self.buffer_patch_sb_counters(&mut buf, -(alloc_total_blocks as i64), 0)?;
        }

        // Phase 5: commit everything atomically (journaled if available).
        self.commit_block_buffer(buf)?;
        Ok(new_size)
    }

    /// Buffer-friendly variant of `finalize_inode_after_write`: patches
    /// size, blocks, ctime, mtime, and checksum on `raw` IN PLACE without
    /// writing to disk. Caller stages the result via `buffer_write_inode`
    /// so the inode update is atomic with the surrounding multi-block tx.
    fn finalize_inode_raw_after_write(
        &self,
        ino: u32,
        raw: &mut [u8],
        orig: &Inode,
        new_size: u64,
        new_sectors: u64,
    ) -> Result<()> {
        Self::patch_inode_size_and_blocks(raw, new_size, new_sectors)?;
        let now = self.runtime.now_unix_seconds();
        set_inode_time(raw, InodeTime::Ctime, now);
        set_inode_time(raw, InodeTime::Mtime, now);
        if self.csum.enabled {
            self.csum.patch_inode_checksum(ino, orig.generation, raw);
        }
        Ok(())
    }

    /// Whether logical block `logical` of directory `dir` is an htree index
    /// block (the dx_root, or a dx_node) rather than a block of entries.
    ///
    /// The kernel's rule in `__ext4_read_dirblock`: in an indexed directory,
    /// block 0 is the root, and a block whose first record is an empty entry
    /// spanning the whole block is a node. Nothing at the END of the block
    /// decides it. A kernel-grown dx_root keeps the bytes of the dirent tail
    /// the directory had before it was indexed, so it ends in what looks
    /// exactly like one (#233).
    pub(crate) fn is_htree_index_block(dir: &Inode, logical: u64, block: &[u8]) -> bool {
        if dir.flags & crate::inode::InodeFlags::INDEX.bits() == 0 || block.len() < 8 {
            return false;
        }
        let first_inode = u32::from_le_bytes(block[0..4].try_into().unwrap());
        let first_len = u16::from_le_bytes(block[4..6].try_into().unwrap()) as usize;
        logical == 0 || (first_inode == 0 && first_len == block.len())
    }

    /// Refuse a directory block whose tail checksum does not verify, before
    /// anything reads entries out of it.
    ///
    /// THE WRITE ENGINE WALKS BLOCKS WITH `DirBlockIter`, WHICH TAKES NO
    /// `Checksummer`. The read path goes through `dir::parse_block_verified`,
    /// which does; the three scans in this file that find the entry a
    /// mutation is about to edit did not. So on a `metadata_csum` volume a
    /// corrupt directory block was refused by `stat` and accepted by
    /// `unlink`, `rename`, `mkdir`, `rmdir`, `link`, `chmod` and the rest.
    ///
    /// AND THE EDIT RE-STAMPED IT. Every one of those paths calls
    /// `patch_dir_entry_tail` after editing the block, computing a fresh and
    /// correct CRC32C over the corrupted contents — so before the write the
    /// damage was detectable and after it, nothing in this crate could see
    /// it. The defect destroyed the evidence of what it had failed to check.
    ///
    /// Same predicate as `dir::parse_block_verified` (`dir.rs`), deliberately:
    /// `csum.enabled` AND a recognisable tail. A volume without the feature,
    /// and a block predating the tail, are both parsed exactly as before.
    fn refuse_unverified_dir_block(
        &self,
        ino: u32,
        dir: &Inode,
        logical: u64,
        block: &[u8],
    ) -> Result<()> {
        // AN INDEX BLOCK IS VERIFIED AS ONE (#233). An htree directory's
        // block 0 is its dx_root and a block whose first record is an empty
        // entry spanning the whole block is a dx_node, and their checksum is
        // the dx_tail's. The kernel's __ext4_read_dirblock tells them apart
        // this way. The dirent-tail test alone does not: when the kernel
        // turns a linear directory into an index it keeps the old tail's
        // bytes in dt_reserved, so a kernel-grown dx_root ENDS in what looks
        // exactly like a dirent tail, whose "checksum" is the index's. Every
        // create in such a directory was refused as a bad directory block.
        if Self::is_htree_index_block(dir, logical, block) {
            // Only the root is verified as an index here. A node is known by
            // its shape alone, and a leaf with no dirent tail whose first
            // record is an empty entry spanning the block has the same shape,
            // so the kernel does not verify a node-shaped block a linear scan
            // reads either (`__ext4_read_dirblock` with `DIRENT`). The
            // htree walk verifies real nodes as it descends (CodeRabbit on
            // #256).
            if logical == 0 {
                return self.check_dx_block(ino, dir, block, true);
            }
            return Ok(());
        }
        if self.csum.enabled
            && crate::dir::has_csum_tail(block)
            && !self.csum.verify_dir_entry_tail(ino, dir.generation, block)
        {
            return Err(Error::BadChecksum {
                what: "directory block",
            });
        }
        Ok(())
    }

    /// Does `name` already exist in `dir_inode`?
    ///
    /// NOT `find_entry_in_dir(..).is_ok()`. That spelling maps EVERY error to
    /// "absent", including the `BadChecksum` this scan now raises — so
    /// `mkdir` on a directory whose block was corrupt concluded the name was
    /// free, added an entry to the block it had just failed to verify, and
    /// re-stamped a valid checksum over it. Verifying the block and then
    /// discarding the verdict is worse than not verifying, because it reads
    /// as protection.
    ///
    /// FOUR SITES DID IT, AND A GREP FINDS THREE. `plan_new_inode_in_dir`,
    /// `apply_mkdir` and `apply_link` wrote `.is_ok()`; `apply_rename` wrote
    /// `.ok()` on the destination check, which discards identically. The
    /// fourth is handled where it is, because rename needs the inode number
    /// rather than a yes/no, but it is the same defect and it is why this
    /// doc comment names the count instead of leaving it to a search.
    ///
    /// Only `NotFound` means absent. Everything else propagates.
    fn entry_exists(&self, dir_ino: u32, dir_inode: &Inode, name: &[u8]) -> Result<bool> {
        match self.find_entry_in_dir(dir_ino, dir_inode, name) {
            Ok(_) => Ok(true),
            Err(Error::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Find `name` in directory `dir_inode` — scans each data block. Returns
    /// the inode number or `Error::NotFound`.
    ///
    /// TAKES THE INODE NUMBER as well as the inode, and only because the
    /// checksum seed needs it: the tail is `crc32c(seed, ino || generation ||
    /// block)`, so a scan that has only the `Inode` cannot verify what it is
    /// reading. Every caller already had the number in scope.
    fn find_entry_in_dir(&self, dir_ino: u32, dir_inode: &Inode, name: &[u8]) -> Result<u32> {
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
        let bs = self.sb.block_size();
        let n_blocks = dir_inode.size.div_ceil(bs as u64);
        for logical in 0..n_blocks {
            let Some(phys) = self.map_inode_logical(dir_inode, logical)? else {
                continue;
            };
            let block = self.read_block(phys)?;
            self.refuse_unverified_dir_block(dir_ino, dir_inode, logical, &block)?;
            for entry in crate::dir::DirBlockIter::new(&block, has_ft) {
                let e = entry?;
                if e.name == name {
                    return Ok(e.inode);
                }
            }
        }
        Err(Error::NotFound)
    }

    /// Apply per-group counter deltas on disk for group `gi`. Positive deltas
    /// increase the corresponding `bg_free_*` / `bg_used_dirs` counter,
    /// negative deltas decrease. Recomputes the BGD csum when `metadata_csum`
    /// is enabled. The in-memory `self.groups` copy is NOT updated — callers
    /// doing a sequence of allocations should `Filesystem::mount` fresh.
    pub(crate) fn patch_bgd_counters(
        &self,
        gi: usize,
        free_blocks_delta: i32,
        free_inodes_delta: i32,
        used_dirs_delta: i32,
    ) -> Result<()> {
        let bs = self.sb.block_size() as u64;
        // Where the descriptor lives, META_BG or not (#73).
        let (bgt_block, off_in_block) = self.sb.descriptor_location(gi as u64);

        let mut block = self.read_block(bgt_block)?;
        patch_bgd_counter_fields(
            &mut block,
            off_in_block,
            self.sb.desc_size,
            free_blocks_delta,
            free_inodes_delta,
            used_dirs_delta,
        );

        self.restamp_group_desc_csum(&mut block[..], off_in_block, gi);
        self.dev.write_at(bgt_block * bs, &block)?;
        Ok(())
    }

    /// Apply deltas to SB `s_free_blocks_count` and `s_free_inodes_count`.
    /// Recomputes the SB checksum when enabled. Does not mutate `self.sb`.
    pub(crate) fn patch_sb_counters(
        &self,
        free_blocks_delta: i64,
        free_inodes_delta: i32,
    ) -> Result<()> {
        // Route through the cache-coherent buffer path (which reads the SB via
        // read_block) so consecutive ops accumulate against the CURRENT
        // on-disk superblock. The old body re-read the immutable mount-time
        // snapshot `self.sb.raw` every call, so within a single mount each
        // call rewrote the SB from mount-time values — a sequence of
        // frees/allocs clobbered each other (e.g. directory growth froze
        // free_blocks at mount-1 and reset free_inodes, which e2fsck flags as
        // "Free blocks/inodes count wrong").
        let mut buf = BlockBuffer::new(self.sb.block_size());
        self.buffer_patch_sb_counters(&mut buf, free_blocks_delta, free_inodes_delta)?;
        self.commit_block_buffer(buf)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // mkdir / rmdir
    // -----------------------------------------------------------------------

    /// Build an on-disk inode image for a freshly-created directory. Sets
    /// `S_IFDIR | mode`, `i_links_count = 2` (for `.` and the dir entry in
    /// the parent), `i_size = block_size` (one data block), EXTENTS flag
    /// with a single leaf extent mapping logical 0 → `data_phys_block`,
    /// timestamps = now.
    fn build_directory_inode(&self, ino: u32, mode: u16, data_phys_block: u64) -> Result<Vec<u8>> {
        use crate::inode::{
            OFF_BLOCKS_HI, OFF_BLOCKS_LO, OFF_LINKS_COUNT, OFF_MODE, OFF_SIZE_HI, OFF_SIZE_LO,
        };
        let mut raw = vec![0u8; self.sb.inode_size as usize];

        let mode_bits = crate::inode::S_IFDIR | (mode & 0x0FFF);
        raw[OFF_MODE..OFF_MODE + 2].copy_from_slice(&mode_bits.to_le_bytes());
        // 2 hard links: one for "." and one for the parent's entry naming this dir.
        raw[OFF_LINKS_COUNT..OFF_LINKS_COUNT + 2].copy_from_slice(&2u16.to_le_bytes());
        self.map_one_block(&mut raw, data_phys_block)?;

        // Size = block_size (the single data block fills the dir).
        let bs = self.sb.block_size() as u64;
        raw[OFF_SIZE_LO..OFF_SIZE_LO + 4]
            .copy_from_slice(&((bs & 0xFFFF_FFFF) as u32).to_le_bytes());
        raw[OFF_SIZE_HI..OFF_SIZE_HI + 4].copy_from_slice(&((bs >> 32) as u32).to_le_bytes());

        let sectors = bs / 512;
        raw[OFF_BLOCKS_LO..OFF_BLOCKS_LO + 4].copy_from_slice(&(sectors as u32).to_le_bytes());
        raw[OFF_BLOCKS_HI..OFF_BLOCKS_HI + 2]
            .copy_from_slice(&(((sectors >> 32) & 0xFFFF) as u16).to_le_bytes());

        // i_extra_isize first: it says whether the epoch bits have a home.
        write_inode_extra_isize(&mut raw);
        let now = self.runtime.now_unix_seconds();
        write_inode_timestamps(&mut raw, now);
        let generation = self.runtime.next_inode_generation();
        write_inode_generation(&mut raw, generation);
        self.stamp_inode_checksum(&mut raw, ino, generation);
        Ok(raw)
    }

    /// Seed a freshly-allocated dir block with the two canonical entries
    /// `.` (→ new_ino) and `..` (→ parent_ino). Handles the metadata-csum
    /// tail when required: the last 12 bytes are reserved, and the CRC is
    /// computed over everything before them.
    fn seed_directory_block(
        &self,
        new_ino: u32,
        parent_ino: u32,
        new_generation: u32,
    ) -> Result<Vec<u8>> {
        let bs = self.sb.block_size() as usize;
        let mut block = vec![0u8; bs];
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
        let reserved_tail = if self.csum.enabled { 12 } else { 0 };
        let usable = bs - reserved_tail;

        // "." entry: rec_len = 12
        block[0..4].copy_from_slice(&new_ino.to_le_bytes());
        block[4..6].copy_from_slice(&12u16.to_le_bytes());
        block[6] = 1; // name_len
        block[7] = if has_ft {
            crate::dir::DirEntryType::Directory as u8
        } else {
            0
        };
        block[8] = b'.';

        // ".." entry: rec_len absorbs the rest of the usable region.
        let off = 12;
        block[off..off + 4].copy_from_slice(&parent_ino.to_le_bytes());
        let rec_len = (usable - off) as u16;
        block[off + 4..off + 6].copy_from_slice(&rec_len.to_le_bytes());
        block[off + 6] = 2;
        block[off + 7] = if has_ft {
            crate::dir::DirEntryType::Directory as u8
        } else {
            0
        };
        block[off + 8] = b'.';
        block[off + 9] = b'.';

        // Tail (when metadata_csum enabled): fake inode=0, rec_len=12,
        // name_len=0, file_type=0xDE, u32 checksum.
        if reserved_tail == 12 {
            self.csum
                .patch_dir_entry_tail(new_ino, new_generation, &mut block);
        }

        Ok(block)
    }

    /// Whether the volume carries RO_COMPAT `DIR_NLINK`, under which a
    /// directory `i_links_count` of 1 means "more subdirectories than the
    /// field can count".
    fn has_dir_nlink(&self) -> bool {
        self.sb.feature_ro_compat & features::RoCompat::DIR_NLINK.bits() != 0
    }

    /// Refuse, with [`Error::TooManyLinks`], a new link to `inode` that the
    /// link count has no room for: the kernel's `ext4_link` check for files
    /// and `EXT4_DIR_LINK_MAX` for a directory gaining a subdirectory.
    ///
    /// Called before anything is staged, so a refusal writes nothing.
    fn check_link_room(&self, inode: &Inode) -> Result<()> {
        next_links_count(inode.is_dir(), inode.links_count, 1, self.has_dir_nlink()).map(|_| ())
    }

    /// Adjust `i_links_count` on a raw inode image. Recomputes CSUM.
    ///
    /// The count moves as the kernel's `ext4_inc_count` / `ext4_dec_count`
    /// move it (see [`next_links_count`]): it never wraps, and a directory
    /// count of 1 stays 1.
    fn patch_inode_nlink(&self, ino: u32, raw: &mut [u8], inode: &Inode, delta: i32) -> Result<()> {
        let new_count = next_links_count(
            inode.is_dir(),
            inode.links_count,
            delta,
            self.has_dir_nlink(),
        )?;
        raw[0x1A..0x1C].copy_from_slice(&new_count.to_le_bytes());
        if self.csum.enabled {
            self.csum.patch_inode_checksum(ino, inode.generation, raw);
        }
        Ok(())
    }

    /// Create a subdirectory at `path` with POSIX mode bits (low 12 bits of
    /// `mode`). Returns the new directory's inode number. Steps: allocate
    /// inode (Orlov-hinted) → allocate one data block → seed it with `.` / `..`
    /// → build dir inode → write inode + data block → add dir entry in parent
    /// → bump parent's `i_links_count` → commit BGD/SB counters.
    ///
    /// Not journaled — safe only in scratch-image contexts until transaction
    /// wrapping lands.
    pub fn apply_mkdir(&self, path: &str, mode: u16) -> Result<u32> {
        self.apply_mkdir_bytes(path.as_bytes(), mode)
    }

    /// [`apply_mkdir`](Self::apply_mkdir) of a path given as bytes, never decoded.
    pub(crate) fn apply_mkdir_bytes(&self, path: &[u8], mode: u16) -> Result<u32> {
        self.refuse_write()?;
        let (parent, name) = self.resolve_new_parent(path)?;
        self.apply_mkdir_at(parent, name, mode)
    }

    /// [`apply_mkdir`](Self::apply_mkdir) of entry `name` in directory
    /// `dir`. `name` is bytes and need not be UTF-8.
    pub fn apply_mkdir_at(&self, dir: impl Into<InodeRef>, name: &[u8], mode: u16) -> Result<u32> {
        self.refuse_write()?;
        check_new_entry_name(name)?;
        let dir = dir.into();
        let parent_ino = dir.ino;
        let (parent_inode, mut parent_raw) = self.live_dir(dir)?;
        if self.entry_exists(parent_ino, &parent_inode, name)? {
            return Err(Error::AlreadyExists);
        }
        // The new subdirectory's `..` is a link to the parent.
        self.check_link_room(&parent_inode)?;

        let bs = self.sb.block_size();
        let parent_group = (parent_ino - 1) / self.sb.inodes_per_group;
        let mut bitmap_reader = |block: u64| self.read_block(block);

        // 1. Allocate inode (is_dir = true so Orlov picks a dir-friendly group).
        let iplan = crate::alloc::plan_inode_allocation(
            &self.sb,
            &self.allocation_groups(),
            true,
            parent_group,
            &mut bitmap_reader,
        )?;
        let new_ino = iplan.inode;

        // 2. Allocate one data block for the dir contents.
        let bplan = crate::alloc::plan_block_allocation(
            &self.sb,
            &self.allocation_groups(),
            1,
            iplan.bgd.group_idx,
            &mut bitmap_reader,
        )?;
        let data_block = bplan.first_block;

        // Multi-block transaction: inode bitmap + block bitmap + counters
        // + new dir inode + seeded data block + parent dir entry +
        // parent nlink bump, all atomic.
        let mut buf = BlockBuffer::new(bs);
        self.buffer_mark_inode_used(&mut buf, new_ino)?;
        self.buffer_patch_bgd_counters(
            &mut buf,
            iplan.bgd.group_idx as usize,
            iplan.bgd.free_blocks_delta,
            iplan.bgd.free_inodes_delta,
            iplan.bgd.used_dirs_delta,
        )?;
        self.buffer_patch_sb_counters(
            &mut buf,
            iplan.sb.free_blocks_delta,
            iplan.sb.free_inodes_delta,
        )?;

        self.buffer_mark_block_run_used(&mut buf, data_block, 1)?;
        self.buffer_patch_bgd_counters(
            &mut buf,
            bplan.bgd.group_idx as usize,
            bplan.bgd.free_blocks_delta,
            bplan.bgd.free_inodes_delta,
            bplan.bgd.used_dirs_delta,
        )?;
        self.buffer_patch_sb_counters(
            &mut buf,
            bplan.sb.free_blocks_delta,
            bplan.sb.free_inodes_delta,
        )?;

        let raw = self.build_directory_inode(new_ino, mode, data_block)?;
        let gen = u32::from_le_bytes(raw[0x64..0x68].try_into().unwrap());
        self.buffer_write_inode(&mut buf, new_ino, &raw)?;

        // Seed the data block (`.` and `..` entries) and stage it.
        let seed = self.seed_directory_block(new_ino, parent_ino, gen)?;
        buf.put(data_block, seed);

        // Try to install the dir entry in the parent in-place first.
        let parent_extends = match self.buffer_add_dir_entry_inplace(
            &mut buf,
            parent_ino,
            &parent_inode,
            name,
            new_ino,
            crate::dir::DirEntryType::Directory,
        ) {
            Ok(()) => false,
            Err(Error::OutOfBounds) => true,
            Err(e) => return Err(e),
        };

        if !parent_extends {
            // In-place add succeeded — bump parent's nlink in the same buffer.
            self.patch_inode_nlink(parent_ino, &mut parent_raw, &parent_inode, 1)?;
            self.buffer_write_inode(&mut buf, parent_ino, &parent_raw)?;
            self.commit_block_buffer(buf)?;
        } else {
            // Parent dir is full → grow it, and bump its nlink, in the
            // same transaction. The parent is re-read from the buffer,
            // where the growth restaged it.
            self.extend_dir_and_add_entry(
                &mut buf,
                parent_ino,
                name,
                new_ino,
                crate::dir::DirEntryType::Directory,
            )?;
            let (grown_parent, mut grown_raw) = self.buffered_inode_verified(&buf, parent_ino)?;
            self.patch_inode_nlink(parent_ino, &mut grown_raw, &grown_parent, 1)?;
            self.buffer_write_inode(&mut buf, parent_ino, &grown_raw)?;
            self.commit_block_buffer(buf)?;
        }

        Ok(new_ino)
    }

    /// Create a hard link at `dst` pointing to the same inode as `src`.
    ///
    /// Semantics:
    /// - `src` must exist and must NOT be a directory (POSIX forbids
    ///   directory hardlinks to avoid reference cycles).
    /// - `dst`'s parent must exist and be a directory.
    /// - `dst` must not already exist.
    /// - On success the shared inode's `i_links_count` is incremented by 1.
    ///
    /// Not journaled — same caveat as other Phase-4 ops.
    pub fn apply_link(&self, src: &str, dst: &str) -> Result<()> {
        self.apply_link_bytes(src.as_bytes(), dst.as_bytes())
    }

    /// [`apply_link`](Self::apply_link) of a path given as bytes, never decoded.
    pub(crate) fn apply_link_bytes(&self, src: &[u8], dst: &[u8]) -> Result<()> {
        self.refuse_write()?;
        let (dst_parent_path, dst_name) = split_parent_and_base(dst)?;
        if dst_name.len() > 255 {
            return Err(Error::NameTooLong);
        }
        let src_ino = self.resolve(src)?;
        // Before the destination is resolved, as it always was: linking a
        // directory is EISDIR wherever it was to go.
        if self.live_inode(src_ino.into())?.0.is_dir() {
            return Err(Error::IsADirectory);
        }
        let dst_parent = self.resolve(dst_parent_path)?;
        self.apply_link_at(src_ino, dst_parent, dst_name)
    }

    /// [`apply_link`](Self::apply_link): a new entry `dst_name` in
    /// directory `dir` for the inode `target` names.
    pub fn apply_link_at(
        &self,
        target: impl Into<InodeRef>,
        dir: impl Into<InodeRef>,
        dst_name: &[u8],
    ) -> Result<()> {
        self.refuse_write()?;
        check_new_entry_name(dst_name)?;
        let target = target.into();
        let src_ino = target.ino;
        let (src_inode, mut src_raw) = self.live_inode(target)?;
        if src_inode.is_dir() {
            // POSIX: hard-linking a directory is forbidden. Map to EISDIR
            // (rather than EPERM) — matches our IsADirectory convention.
            return Err(Error::IsADirectory);
        }
        self.check_link_room(&src_inode)?;

        let dir = dir.into();
        let dst_parent_ino = dir.ino;
        let (dst_parent_inode, _) = self.live_dir(dir)?;
        if self.entry_exists(dst_parent_ino, &dst_parent_inode, dst_name)? {
            return Err(Error::AlreadyExists);
        }

        let dir_type = crate::dir::DirEntryType::from_mode(src_inode.mode);

        // Build the multi-block transaction: bump nlink + add dir entry,
        // both staged into one buffer so a crash either applies both or
        // neither.
        let mut buf = BlockBuffer::new(self.sb.block_size());
        self.patch_inode_nlink(src_ino, &mut src_raw, &src_inode, 1)?;
        self.buffer_write_inode(&mut buf, src_ino, &src_raw)?;

        match self.buffer_add_dir_entry_inplace(
            &mut buf,
            dst_parent_ino,
            &dst_parent_inode,
            dst_name,
            src_ino,
            dir_type,
        ) {
            Ok(()) => self.commit_block_buffer(buf),
            Err(Error::OutOfBounds) => {
                // Parent dir is full → grow it in the same transaction as
                // the nlink bump.
                self.extend_dir_and_add_entry(
                    &mut buf,
                    dst_parent_ino,
                    dst_name,
                    src_ino,
                    dir_type,
                )?;
                self.commit_block_buffer(buf)
            }
            Err(e) => Err(e),
        }
    }

    /// Rename `src` → `dst` within the same filesystem.
    ///
    /// Semantics:
    /// - Both endpoints are within this mount.
    /// - Works for files and directories.
    /// - Cross-parent moves update the moved dir's `..` entry + bump /
    ///   decrement both parents' `i_links_count`.
    /// - Refuses to move a directory into its own subtree (cycle check).
    /// - Same source and dest: no-op success.
    /// - When dst already exists:
    ///     - `replace_if_exists = false` → returns `Error::AlreadyExists`.
    ///     - `replace_if_exists = true` → overwrites dst. See
    ///       "Atomicity" below for exactly how far that holds.
    ///       Type-compatibility rules (POSIX rename(2)):
    ///         * file→dir   → `Error::IsADirectory`
    ///         * dir→file   → `Error::NotADirectory`
    ///         * non-empty-dir overwrite → `Error::DirectoryNotEmpty`
    ///         * src and dst resolve to the same inode (hardlink) →
    ///           no-op success.
    ///       Otherwise the previous dst inode's link count is decremented
    ///       in the same buffer; if that drops it to zero the inode's
    ///       extents and slot are freed in the same atomic commit.
    ///
    /// # Atomicity
    ///
    /// Both paths stage their work into a single [`BlockBuffer`] and
    /// commit it through the journal, so a crash either applies the
    /// whole rename or none of it. That includes growing the destination
    /// directory, splitting a full leaf of its htree index (#302), and
    /// dropping that index when it has no room to route a new leaf (#347).
    pub fn apply_rename(&self, src: &str, dst: &str, replace_if_exists: bool) -> Result<()> {
        self.apply_rename_bytes(src.as_bytes(), dst.as_bytes(), replace_if_exists)
    }

    /// [`apply_rename`](Self::apply_rename) of a path given as bytes, never decoded.
    pub(crate) fn apply_rename_bytes(
        &self,
        src: &[u8],
        dst: &[u8],
        replace_if_exists: bool,
    ) -> Result<()> {
        // The verdict only: the volume is marked not clean once the rename
        // is known to write (#303), in `apply_rename_at`.
        self.write_refusal()?;
        let (src_parent_path, src_name) = split_parent_and_base(src)?;
        let (dst_parent_path, dst_name) = split_parent_and_base(dst)?;
        if dst_name.len() > 255 {
            return Err(Error::NameTooLong);
        }
        let src_parent = self.resolve(src_parent_path)?;
        let dst_parent = self.resolve(dst_parent_path)?;
        self.apply_rename_at(
            src_parent,
            src_name,
            dst_parent,
            dst_name,
            replace_if_exists,
        )
    }

    /// [`apply_rename`](Self::apply_rename) of entry `src_name` in directory
    /// `src_dir` to entry `dst_name` in directory `dst_dir`. Renaming an
    /// entry to itself succeeds and writes nothing; `.` and `..` cannot be
    /// renamed or replaced (`Error::InvalidArgument`).
    pub fn apply_rename_at(
        &self,
        src_dir: impl Into<InodeRef>,
        src_name: &[u8],
        dst_dir: impl Into<InodeRef>,
        dst_name: &[u8],
        replace_if_exists: bool,
    ) -> Result<()> {
        // The verdict only; the volume is marked not clean below, once the
        // rename is known to write (#303).
        self.write_refusal()?;
        check_entry_name(src_name)?;
        check_new_entry_name(dst_name)?;
        // Renaming `.` removed the directory's own entry for itself and
        // filed the directory a second time elsewhere.
        if is_dot_or_dotdot(src_name) || is_dot_or_dotdot(dst_name) {
            return Err(Error::InvalidArgument("rename: cannot rename . or .."));
        }

        let (src_dir, dst_dir) = (src_dir.into(), dst_dir.into());
        let (src_parent_ino, dst_parent_ino) = (src_dir.ino, dst_dir.ino);
        let (src_parent_inode, _) = self.live_dir(src_dir)?;
        let (dst_parent_inode, _) = self.live_dir(dst_dir)?;

        let src_ino = self.find_entry_in_dir(src_parent_ino, &src_parent_inode, src_name)?;
        // rename(2) of an existing name onto itself succeeds and changes
        // nothing. Only after both names are validated and the source is
        // found: a NUL name is still refused and a missing one is still
        // ENOENT (#303).
        if src_parent_ino == dst_parent_ino && src_name == dst_name {
            return Ok(());
        }
        self.mark_not_clean_once()?;
        // `.ok()` here for the same reason as `entry_exists` above: it turned
        // a refusal to read the block into "dst does not exist", and rename
        // then created it and re-stamped the block.
        let existing_dst_ino =
            match self.find_entry_in_dir(dst_parent_ino, &dst_parent_inode, dst_name) {
                Ok(ino) => Some(ino),
                Err(Error::NotFound) => None,
                Err(e) => return Err(e),
            };
        if existing_dst_ino.is_some() && !replace_if_exists {
            return Err(Error::AlreadyExists);
        }

        let (src_inode, _) = self.live_inode(src_ino.into())?;
        let src_is_dir = src_inode.is_dir();

        // Cycle check: moving a directory into its own subtree would cut
        // that subtree off from the root. Walked up the destination's `..`
        // chain, so it holds however the caller named the two directories.
        if src_is_dir && self.is_ancestor_or_self(src_ino, dst_parent_ino)? {
            return Err(Error::InvalidArgument(
                "rename: cannot move directory into its own subtree",
            ));
        }

        // A directory arriving in a new parent adds a `..` link to it; one
        // that replaces a directory there leaves the count where it was.
        if src_is_dir && existing_dst_ino.is_none() && src_parent_ino != dst_parent_ino {
            self.check_link_room(&dst_parent_inode)?;
        }

        // Map POSIX mode bits to the directory-entry file-type byte: every
        // type, not only the three this once knew, which filed a renamed
        // FIFO, socket or device node under type 0 (#386).
        let dir_type = crate::dir::DirEntryType::from_mode(src_inode.mode);

        // ===================================================================
        // Replace-overwrite branch — dst already exists and caller opted in.
        // ===================================================================
        if let Some(dst_old_ino) = existing_dst_ino {
            // Hardlink case: src and dst already share an inode. POSIX
            // rename(2) requires this to be a no-op success — entry count
            // is unchanged, and removing src would unconditionally drop the
            // shared link count by one which is wrong.
            if dst_old_ino == src_ino {
                return Ok(());
            }

            let (dst_old_inode, mut dst_old_raw) = self.read_inode_verified(dst_old_ino)?;
            let dst_is_dir = dst_old_inode.is_dir();

            // Type compatibility — rename(2) forbids crossing the
            // file/directory boundary.
            if !src_is_dir && dst_is_dir {
                return Err(Error::IsADirectory);
            }
            if src_is_dir && !dst_is_dir {
                return Err(Error::NotADirectory);
            }

            // Non-empty-dir overwrite is forbidden by POSIX. Walk every
            // block of dst and reject any entry that isn't `.` / `..`.
            if dst_is_dir {
                let bs = self.sb.block_size();
                let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
                let blocks = dst_old_inode.size.div_ceil(bs as u64);
                for logical in 0..blocks {
                    // Either mapping, as rmdir's emptiness check.
                    let Some(phys) = self.map_inode_logical(&dst_old_inode, logical)? else {
                        continue;
                    };
                    let block = self.read_block(phys)?;
                    // The block this branch is about to overwrite. Unverified
                    // here, it would be emptied and re-stamped valid.
                    self.refuse_unverified_dir_block(dst_old_ino, &dst_old_inode, logical, &block)?;
                    for entry in crate::dir::DirBlockIter::new(&block, has_ft) {
                        let e = entry?;
                        if e.name != b"." && e.name != b".." {
                            return Err(Error::DirectoryNotEmpty);
                        }
                    }
                }
            }

            // Validate a destination that will be freed before any directory
            // growth can commit part of the rename.
            let destination_runs =
                if (dst_is_dir || dst_old_inode.links_count <= 1) && dst_old_inode.has_extents() {
                    self.extent_tree_runs(dst_old_ino, &dst_old_inode)?
                } else {
                    Vec::new()
                };

            // Stage the whole overwrite into a single buffer so a crash
            // either fully replaces dst or leaves the FS in its prior
            // state — UNLESS the destination's htree index has to be
            // dropped, which commits this buffer early. See the
            // "Atomicity" section on this function for what that costs.
            let mut buf = BlockBuffer::new(self.sb.block_size());

            // Parent link-count changes are ACCUMULATED rather than
            // applied where they are discovered.
            //
            // Each site used to read its parent inode back from disk and
            // stage a write of it. Two such sites naming the same inode
            // in one buffer would have the second read stale bytes and
            // overwrite the first's change — and the only thing
            // preventing that was that their branch conditions happened
            // to be mutually exclusive, which nothing said and nothing
            // enforced.
            //
            // Summing deltas and applying them once removes the hazard
            // instead of relying on it not being reached: every parent
            // is read exactly once, after every delta is known, and
            // written exactly once. It also turns the dir-replaces-dir
            // "these two cancel out" reasoning into arithmetic that
            // cancels, rather than a suppressed branch that has to be
            // kept in step with the branch it suppresses.
            let mut parent_nlink: BTreeMap<u32, i32> = BTreeMap::new();

            // 1. Pop the existing dst entry from dst_parent so the
            //    in-place add below has somewhere to land.
            self.buffer_remove_dir_entry(&mut buf, dst_parent_ino, &dst_parent_inode, dst_name)?;

            // 2. Add the new dst entry pointing at src_ino. Try in-place
            //    first; if no block has room, mirror the dst_extends
            //    fall-back from the non-replace path.
            let dst_extends = match self.buffer_add_dir_entry_inplace(
                &mut buf,
                dst_parent_ino,
                &dst_parent_inode,
                dst_name,
                src_ino,
                dir_type,
            ) {
                Ok(()) => false,
                Err(Error::OutOfBounds) => true,
                Err(e) => return Err(e),
            };
            if dst_extends {
                self.extend_dir_and_add_entry(
                    &mut buf,
                    dst_parent_ino,
                    dst_name,
                    src_ino,
                    dir_type,
                )?;
            }

            // 3. Remove src entry from its parent, as step 2 left it: read
            //    through the buffer, for the same reason as below (#392).
            let (src_parent_now, _) = self.buffered_inode_verified(&buf, src_parent_ino)?;
            self.buffer_remove_dir_entry(&mut buf, src_parent_ino, &src_parent_now, src_name)?;

            // 4. Cross-parent dir move: fix `..` + parent nlinks.
            //    For dir-replaces-dir the dst_parent gains the moved
            //    subdir and loses the dropped one; both deltas are
            //    recorded and cancel in the sum.
            if src_is_dir && src_parent_ino != dst_parent_ino {
                self.buffer_update_dotdot(&mut buf, src_ino, &src_inode, dst_parent_ino)?;
                *parent_nlink.entry(src_parent_ino).or_default() -= 1;
                *parent_nlink.entry(dst_parent_ino).or_default() += 1;
            }

            // 5. Decrement dst_old_ino's link count. If it hits zero,
            //    free its data extents + inode slot in this same buffer.
            //    Directories always reap (they only ever have one external
            //    name in our v1 — directory hardlinks aren't supported).
            let new_links = dst_old_inode.links_count.saturating_sub(1);
            if new_links > 0 && !dst_is_dir {
                // Hardlinked file overwrite — just persist the new count.
                dst_old_raw[0x1A..0x1C].copy_from_slice(&new_links.to_le_bytes());
                self.finalize_inode_raw(dst_old_ino, dst_old_inode.generation, &mut dst_old_raw)?;
                self.buffer_write_inode(&mut buf, dst_old_ino, &dst_old_raw)?;
            } else {
                let bs = self.sb.block_size();
                let sectors_per_block = bs as u64 / 512;
                let mut freed_sectors: u64 = 0;
                if dst_old_inode.has_extents() {
                    freed_sectors +=
                        self.buffer_free_runs(&mut buf, &destination_runs)? * sectors_per_block;
                } else {
                    // A block-mapped file or directory being replaced: its
                    // blocks were left allocated.
                    freed_sectors +=
                        self.buffer_free_block_map(&mut buf, &dst_old_inode)? * sectors_per_block;
                }
                if dst_old_inode.file_acl != 0 {
                    freed_sectors += self
                        .buffer_release_xattr_block(&mut buf, dst_old_inode.file_acl)?
                        * sectors_per_block;
                }

                self.buffer_free_inode_slot(&mut buf, dst_old_ino)?;
                if dst_is_dir {
                    // Reaped a directory → bg_used_dirs_count -= 1.
                    let dst_old_gi = ((dst_old_ino - 1) / self.sb.inodes_per_group) as usize;
                    self.buffer_patch_bgd_counters(&mut buf, dst_old_gi, 0, 0, -1)?;
                }
                let freed_blocks = freed_sectors.checked_div(sectors_per_block).unwrap_or(0);
                self.buffer_patch_sb_counters(&mut buf, freed_blocks as i64, 1)?;

                // Zero the inode body, set dtime = now, preserve generation.
                let inode_size = self.sb.inode_size as usize;
                let old_gen = dst_old_inode.generation;
                for b in &mut dst_old_raw[..inode_size] {
                    *b = 0;
                }
                let dtime = self.dtime_now();
                dst_old_raw[0x14..0x18].copy_from_slice(&dtime.to_le_bytes());
                dst_old_raw[0x64..0x68].copy_from_slice(&old_gen.to_le_bytes());
                self.finalize_inode_raw(dst_old_ino, old_gen, &mut dst_old_raw)?;
                self.buffer_write_inode(&mut buf, dst_old_ino, &dst_old_raw)?;

                // Dir-replaces-dir: dst_parent loses the removed subdir's
                // `..` reference → -1 nlink. Recorded unconditionally;
                // when a cross-parent dir move already recorded a +1 for
                // the same parent, the sum is what cancels them.
                if dst_is_dir {
                    *parent_nlink.entry(dst_parent_ino).or_default() -= 1;
                }
            }

            self.apply_parent_nlink_deltas(&mut buf, &parent_nlink)?;
            return self.commit_block_buffer(buf);
        }

        // ===================================================================
        // No-overwrite path — dst doesn't exist. Mirrors the v1 behaviour.
        // ===================================================================
        // Multi-block transaction: insert dst entry + remove src entry +
        // (cross-parent dir) update .. + adjust parent nlinks. Atomic so
        // a crash either fully renames or leaves the original, including
        // when the destination's htree index has to be dropped (#347).
        let mut buf = BlockBuffer::new(self.sb.block_size());
        let mut parent_nlink: BTreeMap<u32, i32> = BTreeMap::new();

        let dst_extends = match self.buffer_add_dir_entry_inplace(
            &mut buf,
            dst_parent_ino,
            &dst_parent_inode,
            dst_name,
            src_ino,
            dir_type,
        ) {
            Ok(()) => false,
            Err(Error::OutOfBounds) => true,
            Err(e) => return Err(e),
        };

        if dst_extends {
            // Dest parent full → grow it in this transaction.
            self.extend_dir_and_add_entry(&mut buf, dst_parent_ino, dst_name, src_ino, dir_type)?;
        }

        // The source parent as the add above left it, read through the
        // buffer. When it is also the destination and the add split a full
        // htree leaf, the upper half of that leaf -- the source entry among
        // it, perhaps -- now lives in a block past the size read before the
        // add, and removing through that stale inode missed it: a legal
        // rename failed with NotFound (#392). When the add dropped the index
        // instead (#347), the stale inode still called it indexed.
        let (src_parent_now, _) = self.buffered_inode_verified(&buf, src_parent_ino)?;
        self.buffer_remove_dir_entry(&mut buf, src_parent_ino, &src_parent_now, src_name)?;

        if src_is_dir && src_parent_ino != dst_parent_ino {
            self.buffer_update_dotdot(&mut buf, src_ino, &src_inode, dst_parent_ino)?;
            *parent_nlink.entry(src_parent_ino).or_default() -= 1;
            *parent_nlink.entry(dst_parent_ino).or_default() += 1;
        }

        // Read after the extend above, if there was one, so the counts
        // come from what is actually on disk now.
        self.apply_parent_nlink_deltas(&mut buf, &parent_nlink)?;
        self.commit_block_buffer(buf)
    }

    /// True if directory `ancestor` is `dir` or lies on its `..` chain to
    /// the root.
    fn is_ancestor_or_self(&self, ancestor: u32, dir: u32) -> Result<bool> {
        let mut cur = dir;
        // A `..` chain longer than the inode table is a loop, not a tree.
        for _ in 0..=self.sb.inodes_count {
            if cur == ancestor {
                return Ok(true);
            }
            if cur == crate::path::EXT4_ROOT_INODE {
                return Ok(false);
            }
            let (inode, _) = self.read_inode_verified(cur)?;
            cur = crate::path::find_entry(
                self.dev.as_ref(),
                &self.sb,
                cur,
                &inode,
                b"..",
                &self.csum,
            )?;
        }
        Err(Error::Corrupt(
            "directory `..` chain does not reach the root",
        ))
    }

    /// Apply accumulated `i_links_count` deltas, one read and one write
    /// per inode.
    ///
    /// The point is the "one read" half. Patching a link count means
    /// reading the inode, changing the field and staging the whole
    /// record — so two patches of the same inode staged into one buffer
    /// would have the second read the *pre-buffer* bytes from disk and
    /// write them back over the first. Summing first makes that
    /// impossible rather than merely unreached.
    ///
    /// A delta of zero writes nothing. That is what makes the
    /// dir-replaces-dir case (+1 for the arriving subdirectory, -1 for
    /// the departing one) come out as no write at all, without a branch
    /// anywhere having to know about the other.
    fn apply_parent_nlink_deltas(
        &self,
        buf: &mut BlockBuffer,
        deltas: &BTreeMap<u32, i32>,
    ) -> Result<()> {
        for (&ino, &delta) in deltas {
            if delta == 0 {
                continue;
            }
            let (inode, mut raw) = self.buffered_inode_verified(buf, ino)?;
            self.patch_inode_nlink(ino, &mut raw, &inode, delta)?;
            self.buffer_write_inode(buf, ino, &raw)?;
        }
        Ok(())
    }

    /// Stage one freshly planned directory block as used: its bitmap bit
    /// (through `buffer_mark_block_run_used`, which also refreshes the
    /// block-bitmap checksum -- the bare `mark_block_run_used` +
    /// `patch_*_counters` sequence the directory-grow path used to run left
    /// that csum stale, so e2fsck reported "block bitmap does not match
    /// checksum" once a directory grew a block) and its BGD + SB free-count
    /// deltas.
    ///
    /// The block is the plan's own, and a plan for anything but one block is
    /// refused whole, rather than marking one block of it and applying the
    /// counter deltas of all of them (#333).
    fn buffer_dir_block_alloc(
        &self,
        buf: &mut BlockBuffer,
        plan: &crate::alloc::BlockAllocationPlan,
    ) -> Result<()> {
        if plan.count != 1 {
            return Err(Error::Corrupt(
                "buffer_dir_block_alloc: a directory block plan must allocate exactly one block",
            ));
        }
        self.buffer_mark_block_run_used(buf, plan.first_block, 1)?;
        self.buffer_patch_bgd_counters(
            buf,
            plan.bgd.group_idx as usize,
            plan.bgd.free_blocks_delta,
            plan.bgd.free_inodes_delta,
            plan.bgd.used_dirs_delta,
        )?;
        self.buffer_patch_sb_counters(buf, plan.sb.free_blocks_delta, plan.sb.free_inodes_delta)
    }

    /// Plan one block for directory `parent_ino` against the bitmaps `buf`
    /// has staged, and stage it as used, so the next plan in the same
    /// transaction picks a different one.
    fn buffer_alloc_dir_block(&self, buf: &mut BlockBuffer, parent_ino: u32) -> Result<u64> {
        let parent_group = (parent_ino - 1) / self.sb.inodes_per_group;
        let plan = self.plan_buffered_block_allocation(buf, 1, parent_group)?;
        self.buffer_dir_block_alloc(buf, &plan)?;
        Ok(plan.first_block)
    }

    /// `block`, as `buf` has it staged, or as the device has it.
    fn buffered_block(&self, buf: &BlockBuffer, block: u64) -> Result<Vec<u8>> {
        match buf.dirty.get(&block) {
            Some(bytes) => Ok(bytes.clone()),
            None => self.read_block(block),
        }
    }

    /// [`Self::read_inode_verified`] through `buf`: the inode as the open
    /// transaction has staged it, when it has staged its inode-table block.
    ///
    /// A directory grown inside a caller's transaction is read and restaged
    /// in it, and restaging the device's copy of an inode the caller has
    /// already patched in the buffer would put the old bytes back.
    fn buffered_inode_verified(&self, buf: &BlockBuffer, ino: u32) -> Result<(Inode, Vec<u8>)> {
        let (block, offset) = bgd::locate_inode(&self.sb, &self.groups, ino)?;
        let Some(staged) = buf.dirty.get(&block) else {
            return self.read_inode_verified(ino);
        };
        let off = offset as usize;
        let raw = staged
            .get(off..off + self.sb.inode_size as usize)
            .ok_or(Error::Corrupt("inode slice exceeds block data"))?
            .to_vec();
        let inode = Inode::parse(&raw)?;
        if self.csum.enabled && !self.csum.verify_inode(ino, inode.generation, &raw) {
            return Err(Error::BadChecksum { what: "inode" });
        }
        Ok((inode, raw))
    }

    /// Recompute `raw`'s inode checksum and stage it into `buf`.
    fn buffer_write_dir_inode(
        &self,
        buf: &mut BlockBuffer,
        ino: u32,
        generation: u32,
        raw: &mut [u8],
    ) -> Result<()> {
        if self.csum.enabled {
            self.csum.patch_inode_checksum(ino, generation, raw);
        }
        self.buffer_write_inode(buf, ino, raw)
    }

    /// Add `(name → target_ino)` to `parent_ino` by growing it, staged into
    /// `buf`, the caller's open transaction, which the caller then commits:
    /// the new entry, its inode and the growth land together.
    ///
    /// An indexed directory's full leaf is split (#195), and the split is
    /// staged whole (#302). Where the index cannot take the new leaf, the
    /// index is dropped instead, staged in `buf` too (#347), and the
    /// directory grows as a linear one.
    fn extend_dir_and_add_entry(
        &self,
        buf: &mut BlockBuffer,
        parent_ino: u32,
        name: &[u8],
        target_ino: u32,
        file_type: crate::dir::DirEntryType,
    ) -> Result<()> {
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;

        // An indexed directory reaches here when the leaf its index picks
        // is full. Split that leaf where the index has room for another
        // entry, as the kernel's `ext4_dx_add_entry` does (#195); only
        // where it does not is the index dropped, so the block appended
        // below is one a linear scan finds.
        if self.buffer_split_htree_leaf_and_add_entry(
            buf, parent_ino, name, target_ino, file_type, has_ft,
        )? {
            return Ok(());
        }
        self.buffer_drop_htree_index(buf, parent_ino)?;

        let (parent_inode, _) = self.buffered_inode_verified(buf, parent_ino)?;
        let block = self.seeded_dir_block(
            &parent_inode,
            parent_ino,
            name,
            target_ino,
            file_type,
            has_ft,
        )?;
        self.buffer_append_dir_block(buf, parent_ino, block)
    }

    /// Split the full htree leaf that `name` routes to, add the entry to the
    /// half its hash belongs in, and route the new half from the parent index
    /// block: the kernel's `do_split` and `dx_insert_block` (#195).
    ///
    /// `Ok(false)` when this is not a split it makes, and the caller drops the
    /// index instead, as the kernel's `dx_fallback` does: the directory is not
    /// indexed, or is casefolded or encrypted (hashed some way this crate does
    /// not), the index has more than one interior level, the parent index
    /// block is full, every name in the leaf has one hash, or the new entry
    /// does not fit the half it belongs in. Nothing is staged then.
    ///
    /// Otherwise everything is staged into `buf`: the new leaf's allocation,
    /// its extent, the directory's new size, the new leaf, the halved leaf
    /// and the routing entry. They land in one commit, so a cut anywhere
    /// leaves either the full leaf or both halves routed (#302). The new
    /// leaf used to be appended and written first, outside the transaction,
    /// and a cut before the commit left a block the index never referenced.
    fn buffer_split_htree_leaf_and_add_entry(
        &self,
        buf: &mut BlockBuffer,
        dir_ino: u32,
        name: &[u8],
        target_ino: u32,
        file_type: crate::dir::DirEntryType,
        has_ft: bool,
    ) -> Result<bool> {
        let (dir, _) = self.buffered_inode_verified(buf, dir_ino)?;
        if dir.flags & crate::inode::InodeFlags::INDEX.bits() == 0
            || dir.flags & (EXT4_CASEFOLD_FL | EXT4_ENCRYPT_FL) != 0
        {
            return Ok(false);
        }
        let bs = self.sb.block_size() as usize;
        let physical = |logical: u32| {
            self.map_inode_logical(&dir, u64::from(logical))?
                .ok_or(Error::CorruptDirEntry("htree block is not mapped"))
        };

        let root_phys = physical(0)?;
        let root = self.buffered_block(buf, root_phys)?;
        self.check_dx_block(dir_ino, &dir, &root, true)?;
        let info = crate::htree::parse_root_info(&root)?;
        if info.indirect_levels > 1 {
            return Ok(false);
        }
        let version = crate::hash::effective_version(info.hash_version, self.sb.unsigned_hash());
        let hash = crate::hash::name_hash(name, version, &self.sb.hash_seed).major;

        let (_, root_entries) = crate::htree::parse_root_entries(&root)?;
        let routed = crate::htree::find_entry_for_hash(&root_entries, hash).block;
        let (parent_phys, parent, count_offset, leaf_logical) = if info.indirect_levels == 0 {
            (root_phys, root, 32, routed)
        } else {
            let node_phys = physical(routed)?;
            let node = self.buffered_block(buf, node_phys)?;
            self.check_dx_block(dir_ino, &dir, &node, false)?;
            let (_, entries) = crate::htree::parse_node_entries(&node)?;
            let leaf = crate::htree::find_entry_for_hash(&entries, hash).block;
            (node_phys, node, 8, leaf)
        };
        if leaf_logical == 0 {
            return Err(Error::CorruptDirEntry(
                "htree routes a name to its own root",
            ));
        }

        let leaf_phys = physical(leaf_logical)?;
        let leaf = self.buffered_block(buf, leaf_phys)?;
        let reserved_tail = if self.csum.enabled && crate::dir::has_csum_tail(&leaf) {
            if !self
                .csum
                .verify_dir_entry_tail(dir_ino, dir.generation, &leaf)
            {
                return Err(Error::BadChecksum {
                    what: "directory block",
                });
            }
            12
        } else {
            0
        };
        let Ok(split) = crate::htree_mut::plan_leaf_split(
            &leaf,
            version,
            &self.sb.hash_seed,
            has_ft,
            bs,
            reserved_tail,
        ) else {
            return Ok(false);
        };

        // Where `buffer_append_dir_block` puts the new leaf: a directory's
        // size is whole blocks.
        let new_logical = u32::try_from(dir.size.div_ceil(bs as u64))
            .map_err(|_| Error::Corrupt("directory too large to index another block"))?;
        let parent = match if count_offset == 32 {
            crate::htree_mut::plan_insert_dx_entry_root(
                &parent,
                32,
                split.split_out_hash,
                new_logical,
            )
        } else {
            crate::htree_mut::plan_insert_dx_entry_node(&parent, split.split_out_hash, new_logical)
        } {
            Ok(bytes) => bytes,
            // A full parent, or a bound the parent already routes.
            Err(Error::CorruptExtentTree(_)) => return Ok(false),
            Err(e) => return Err(e),
        };

        let (mut left, mut right) = (split.left_bytes, split.right_bytes);
        let into = if hash >= split.split_out_hash {
            &mut right
        } else {
            &mut left
        };
        match crate::dir::add_entry_to_block(
            into,
            target_ino,
            name,
            file_type,
            has_ft,
            reserved_tail,
        ) {
            Ok(()) => {}
            Err(Error::OutOfBounds) => return Ok(false),
            Err(e) => return Err(e),
        }
        if reserved_tail == 12 {
            for block in [&mut left, &mut right] {
                self.csum
                    .patch_dir_entry_tail(dir_ino, dir.generation, block);
            }
        }
        let mut parent = parent;
        self.csum
            .patch_dx_tail(dir_ino, dir.generation, &mut parent, count_offset);

        // The new leaf, its allocation and extent and the grown inode; then
        // the halved leaf and the routing entry. One transaction.
        self.buffer_append_dir_block(buf, dir_ino, right)?;
        buf.put(leaf_phys, left);
        buf.put(parent_phys, parent);
        Ok(true)
    }

    /// A fresh directory block holding one entry, with its checksum tail.
    fn seeded_dir_block(
        &self,
        parent_inode: &Inode,
        parent_ino: u32,
        name: &[u8],
        target_ino: u32,
        file_type: crate::dir::DirEntryType,
        has_ft: bool,
    ) -> Result<Vec<u8>> {
        let bs = self.sb.block_size() as usize;
        let reserved_tail = if self.csum.enabled { 12 } else { 0 };
        let mut block = vec![0u8; bs];
        block[4..6].copy_from_slice(&((bs - reserved_tail) as u16).to_le_bytes());
        crate::dir::add_entry_to_block(
            &mut block,
            target_ino,
            name,
            file_type,
            has_ft,
            reserved_tail,
        )?;
        if self.csum.enabled {
            self.csum
                .patch_dir_entry_tail(parent_ino, parent_inode.generation, &mut block);
        }
        Ok(block)
    }

    /// Grow `parent_ino`'s directory by one block holding `block`, at logical
    /// block `size / block_size`, staged into `buf`: the allocation, the
    /// mapping, the block itself and the grown inode. Nothing is written
    /// until the caller commits `buf`, so a cut leaves all of it or none.
    /// Leaves any htree index alone.
    fn buffer_append_dir_block(
        &self,
        buf: &mut BlockBuffer,
        parent_ino: u32,
        block: Vec<u8>,
    ) -> Result<()> {
        let bs = self.sb.block_size();
        let bs_u64 = bs as u64;

        let (parent_inode, mut parent_raw) = self.buffered_inode_verified(buf, parent_ino)?;
        if !parent_inode.is_dir() {
            return Err(Error::NotADirectory);
        }
        let new_logical_block = parent_inode.size.div_ceil(bs_u64);
        if parent_inode.flags & crate::inode::InodeFlags::EXTENTS.bits() == 0 {
            return self.buffer_extend_mapped_dir(
                buf,
                parent_ino,
                &parent_inode,
                &mut parent_raw,
                new_logical_block,
                block,
            );
        }

        // 1. Allocate one fs block. Hint to parent's group.
        let parent_group = (parent_ino - 1) / self.sb.inodes_per_group;
        let plan = self.plan_buffered_block_allocation(buf, 1, parent_group)?;
        let new_phys = plan.first_block;

        // 2. Insert extent into parent's inline extent root. If the root is
        //    saturated at depth 0, promote to depth 1 by allocating a fresh
        //    leaf block, moving all entries into it, and writing a single
        //    index entry into the inline root.
        let new_extent = crate::extent::Extent {
            logical_block: new_logical_block as u32,
            length: 1,
            physical_block: new_phys,
            uninitialized: false,
        };
        // If the parent root is already promoted (depth ≥ 1), operate on the
        // leaf block directly instead of the 60-byte inline root. This keeps
        // the inode.block area unchanged; only the leaf-node physical block
        // gets rewritten.
        let root_header = crate::extent::ExtentHeader::parse(&parent_inode.block)?;
        if root_header.depth == 1 {
            return self.buffer_extend_dir_depth1(
                buf,
                parent_ino,
                &parent_inode,
                &mut parent_raw,
                block,
                new_extent,
                plan,
            );
        }
        if root_header.depth > 1 {
            return self.buffer_extend_dir_deep(
                buf,
                parent_ino,
                &parent_inode,
                &mut parent_raw,
                block,
                new_extent,
                plan,
            );
        }

        // The data block is staged as used now, so the leaf-node plan on
        // the promotion path below picks a different one.
        self.buffer_dir_block_alloc(buf, &plan)?;
        let (new_root, promoted) =
            match crate::extent_mut::plan_insert_extent(&parent_inode.block, new_extent) {
                Ok(muts) => {
                    let root = muts
                        .into_iter()
                        .find_map(|m| match m {
                            crate::extent_mut::ExtentMutation::WriteRoot { bytes } => Some(bytes),
                            _ => None,
                        })
                        .ok_or(Error::Corrupt(
                            "extend_dir_and_add_entry: plan produced no WriteRoot",
                        ))?;
                    (root, false)
                }
                Err(Error::CorruptExtentTree(msg)) if msg.contains("LEAF_FULL_NEEDS_PROMOTION") => {
                    // Second allocation: the leaf node block.
                    let leaf_meta_phys = self.buffer_alloc_dir_block(buf, parent_ino)?;
                    let promo = crate::extent_mut::plan_promote_leaf(
                        &parent_inode.block,
                        new_extent,
                        bs as usize,
                        leaf_meta_phys,
                        self.csum.enabled,
                    )?;
                    let mut leaf = promo.leaf_bytes;
                    if self.csum.enabled {
                        self.csum
                            .patch_extent_tail(parent_ino, parent_inode.generation, &mut leaf);
                    }
                    buf.put(leaf_meta_phys, leaf);
                    (promo.new_root_bytes, true)
                }
                Err(e) => return Err(e),
            };
        Self::patch_inode_block_area(&mut parent_raw, &new_root)?;

        // 3. Patch size (+= block_size) and i_blocks. On the promotion path
        //    the inode claims both the data block AND the leaf-node block.
        let blocks_consumed: u64 = 1 + u64::from(promoted);
        let new_size = parent_inode.size + bs_u64;
        let new_blocks = parent_inode.blocks + (bs_u64 / 512) * blocks_consumed;
        Self::patch_inode_size_and_blocks(&mut parent_raw, new_size, new_blocks)?;

        // 4. The inode, with its checksum, and the block.
        self.buffer_write_dir_inode(buf, parent_ino, parent_inode.generation, &mut parent_raw)?;
        buf.put(new_phys, block);
        Ok(())
    }

    /// [`Self::buffer_append_dir_block`] for an ext2/ext3 directory, whose
    /// blocks are named by `i_block`'s twelve direct pointers and then its
    /// single-indirect block. It read that array as an extent header and
    /// refused, so such a directory never grew past its first block (#89).
    /// A directory needing the double-indirect block is refused.
    fn buffer_extend_mapped_dir(
        &self,
        buf: &mut BlockBuffer,
        parent_ino: u32,
        parent_inode: &Inode,
        parent_raw: &mut [u8],
        new_logical_block: u64,
        block: Vec<u8>,
    ) -> Result<()> {
        use crate::inode::OFF_BLOCK;
        const DIRECT: u64 = 12;
        let bs = self.sb.block_size();
        let bs_u64 = bs as u64;
        let per_block = bs_u64 / 4;
        if new_logical_block >= DIRECT + per_block {
            return Err(Error::Unsupported(
                "growing an ext2/ext3 directory past its single-indirect block",
            ));
        }
        // Each allocation is staged as used, so the next picks another.
        let allocate = |buf: &mut BlockBuffer| -> Result<u32> {
            let phys = self.buffer_alloc_dir_block(buf, parent_ino)?;
            u32::try_from(phys)
                .map_err(|_| Error::Corrupt("directory block past a 32-bit block map"))
        };

        let new_phys = allocate(buf)?;
        let mut blocks_consumed = 1u64;
        if new_logical_block < DIRECT {
            let at = OFF_BLOCK + 4 * new_logical_block as usize;
            parent_raw[at..at + 4].copy_from_slice(&new_phys.to_le_bytes());
        } else {
            let slot = OFF_BLOCK + 4 * DIRECT as usize;
            let mut indirect_phys =
                u32::from_le_bytes(parent_raw[slot..slot + 4].try_into().unwrap());
            let mut indirect = if indirect_phys == 0 {
                indirect_phys = allocate(buf)?;
                blocks_consumed += 1;
                parent_raw[slot..slot + 4].copy_from_slice(&indirect_phys.to_le_bytes());
                vec![0u8; bs as usize]
            } else {
                self.buffered_block(buf, u64::from(indirect_phys))?
            };
            let at = 4 * (new_logical_block - DIRECT) as usize;
            indirect[at..at + 4].copy_from_slice(&new_phys.to_le_bytes());
            buf.put(u64::from(indirect_phys), indirect);
        }

        let new_size = parent_inode.size + bs_u64;
        let new_blocks = parent_inode.blocks + (bs_u64 / 512) * blocks_consumed;
        Self::patch_inode_size_and_blocks(parent_raw, new_size, new_blocks)?;
        self.buffer_write_dir_inode(buf, parent_ino, parent_inode.generation, parent_raw)?;
        buf.put(u64::from(new_phys), block);
        Ok(())
    }

    /// Grow a directory whose extent tree is already at depth ≥ 2.
    /// Uses `plan_insert_extent_deep` to navigate and split the tree,
    /// allocating index-node blocks on demand via
    /// `plan_block_allocation_excluding`, which is told about the data
    /// block and about every meta block already handed out -- none of
    /// which is staged as used until the plan has succeeded.
    #[allow(clippy::too_many_arguments)]
    fn buffer_extend_dir_deep(
        &self,
        buf: &mut BlockBuffer,
        parent_ino: u32,
        parent_inode: &Inode,
        parent_raw: &mut [u8],
        block: Vec<u8>,
        new_extent: crate::extent::Extent,
        data_plan: crate::alloc::BlockAllocationPlan,
    ) -> Result<()> {
        let bs = self.sb.block_size();
        let bs_u64 = bs as u64;
        let parent_group = (parent_ino - 1) / self.sb.inodes_per_group;

        // NOTHING HERE IS STAGED AS USED UNTIL THE PLAN SUCCEEDS, SO THE
        // PLANNER HAS TO BE TOLD WHAT IS ALREADY SPOKEN FOR.
        //
        // The planner reads the bitmaps `buf` holds, and this function marks
        // nothing in them until the tree plan is done. So every call sees
        // the same bytes and returns the same block: measured on a fresh
        // 64 MiB image, three consecutive plans gave `517 517 517`.
        //
        // This used to defend itself with one equality test against
        // `data_block`, and the comment above it claimed the closure "skips
        // that block and retries once", which it never did -- it returned
        // `NoSpaceLeftOnDevice`. Both halves were wrong:
        //
        //   - when the planner did return `data_block`, which it does on the
        //     FIRST call because that is what it returned for the data page
        //     moments earlier, the directory grow failed with
        //     `NoSpaceLeftOnDevice` on a nearly empty filesystem;
        //   - when it did not, the test passed, the block went into
        //     `pending_meta`, and the NEXT call returned the same block,
        //     passed the same test and went in again -- two extent-tree
        //     nodes on one physical block, which is silent corruption.
        //
        // Adding `pending_meta` to that test would only turn the second case
        // into more of the first. The reservations go into the bitmap the
        // scan reads instead, via `plan_block_allocation_excluding`, and the
        // equality test is then unnecessary rather than insufficient.
        let mut pending_meta: Vec<crate::alloc::BlockAllocationPlan> = Vec::new();

        let reader = FsBlockReader { fs: self };
        let staged: &BlockBuffer = buf;
        let mut alloc_fn = || -> Result<u64> {
            // RECOMPUTED PER CALL rather than accumulated, so the list
            // handed to the planner is a function of the plans that
            // exist -- one expression to test, and no state to get out
            // of step with `pending_meta`.
            let reserved = crate::alloc::reserved_blocks(&data_plan, &pending_meta);
            let meta_plan =
                self.plan_buffered_block_allocation_excluding(staged, 1, parent_group, &reserved)?;
            pending_meta.push(meta_plan);
            Ok(pending_meta.last().unwrap().first_block)
        };

        let deep_plan = crate::extent_mut::plan_insert_extent_deep(
            &parent_inode.block,
            new_extent,
            bs,
            &reader,
            &mut alloc_fn,
        )?;

        // The plan holds: every block it handed out is staged as used.
        self.buffer_dir_block_alloc(buf, &data_plan)?;
        for plan in &pending_meta {
            self.buffer_dir_block_alloc(buf, plan)?;
        }

        // Tree-meta blocks (rewritten leaves + any new index nodes).
        for (block, mut bytes) in deep_plan.block_writes {
            if self.csum.enabled {
                self.csum
                    .patch_extent_tail(parent_ino, parent_inode.generation, &mut bytes);
            }
            buf.put(block, bytes);
        }

        // Patch inode: root bytes, size (+1 data block), i_blocks.
        Self::patch_inode_block_area(parent_raw, &deep_plan.new_root)?;
        let new_size = parent_inode.size + bs_u64;
        let new_blocks = parent_inode.blocks + (bs_u64 / 512) * (1 + pending_meta.len() as u64);
        Self::patch_inode_size_and_blocks(parent_raw, new_size, new_blocks)?;
        self.buffer_write_dir_inode(buf, parent_ino, parent_inode.generation, parent_raw)?;
        buf.put(data_plan.first_block, block);
        Ok(())
    }

    /// Grow a directory whose extent tree is already at depth 1 (i.e. has
    /// been promoted). The inline root holds a single index entry → one leaf
    /// block. The mutation happens entirely inside the leaf block; the inode
    /// root is unchanged.
    ///
    /// Leaf overflow (>340 entries in a 4 KiB block with csum) falls back to
    /// `buffer_extend_dir_deep`.
    #[allow(clippy::too_many_arguments)]
    fn buffer_extend_dir_depth1(
        &self,
        buf: &mut BlockBuffer,
        parent_ino: u32,
        parent_inode: &Inode,
        parent_raw: &mut [u8],
        block: Vec<u8>,
        new_extent: crate::extent::Extent,
        plan: crate::alloc::BlockAllocationPlan,
    ) -> Result<()> {
        let bs = self.sb.block_size();
        let bs_u64 = bs as u64;

        // ONLY A ROOT WITH ONE INDEX ENTRY HAS A SINGLE LEAF. Once the deep
        // path has split that leaf the root indexes two or more, and the
        // new extent belongs in the last of them; appending it to the
        // first put logical blocks past the split where no lookup
        // descends, and the entry just added was not found.
        if crate::extent::ExtentHeader::parse(&parent_inode.block)?.entries != 1 {
            return self.buffer_extend_dir_deep(
                buf,
                parent_ino,
                parent_inode,
                parent_raw,
                block,
                new_extent,
                plan,
            );
        }

        // Resolve the single index entry in the 60-byte inline root.
        let idx = crate::extent::ExtentIdx::parse(
            &parent_inode.block
                [crate::extent::EXT4_EXT_NODE_SIZE..2 * crate::extent::EXT4_EXT_NODE_SIZE],
        )?;
        let leaf_phys = idx.leaf_block;

        // Read the leaf block + run plan_insert_extent on its 4 KiB buffer.
        // `plan_insert_extent` operates on any depth-0 root — it uses
        // `header.max` for capacity, which was set to (bs-12-4)/12 = 340
        // when the leaf was built by `plan_promote_leaf`.
        let leaf = self.buffered_block(buf, leaf_phys)?;
        // CRC-verify before mutating — if the leaf's tail is corrupt we'd
        // write a false "fixed" version back.
        if self.csum.enabled
            && !self
                .csum
                .verify_extent_tail(parent_ino, parent_inode.generation, &leaf)
        {
            return Err(Error::BadChecksum {
                what: "extent block",
            });
        }

        let muts = match crate::extent_mut::plan_insert_extent(&leaf, new_extent) {
            Ok(muts) => muts,
            Err(Error::CorruptExtentTree(msg)) if msg.contains("LEAF_FULL_NEEDS_PROMOTION") => {
                // The single depth-1 leaf is full (≥340 extents in a 4 KiB block
                // with csum). Fall back to the deep path, which handles adding a
                // sibling leaf or promoting to depth 2. The data block hasn't
                // been staged yet, so pass `plan` unchanged.
                return self.buffer_extend_dir_deep(
                    buf,
                    parent_ino,
                    parent_inode,
                    parent_raw,
                    block,
                    new_extent,
                    plan,
                );
            }
            Err(e) => return Err(e),
        };
        let mut new_leaf = muts
            .into_iter()
            .find_map(|m| match m {
                crate::extent_mut::ExtentMutation::WriteRoot { bytes } => Some(bytes),
                _ => None,
            })
            .ok_or(Error::Corrupt(
                "buffer_extend_dir_depth1: plan produced no WriteRoot",
            ))?;
        if self.csum.enabled {
            self.csum
                .patch_extent_tail(parent_ino, parent_inode.generation, &mut new_leaf);
        }
        self.buffer_dir_block_alloc(buf, &plan)?;
        buf.put(leaf_phys, new_leaf);

        // Inode root is unchanged — just grow size + blocks by one data block.
        let new_size = parent_inode.size + bs_u64;
        let new_blocks = parent_inode.blocks + (bs_u64 / 512);
        Self::patch_inode_size_and_blocks(parent_raw, new_size, new_blocks)?;
        self.buffer_write_dir_inode(buf, parent_ino, parent_inode.generation, parent_raw)?;
        buf.put(plan.first_block, block);
        Ok(())
    }

    /// Remove an empty directory at `path`. Requires the target to contain
    /// only `.` and `..`. Frees the data block(s) + inode, removes the
    /// entry from the parent, decrements parent's `i_links_count`.
    pub fn apply_rmdir(&self, path: &str) -> Result<()> {
        self.apply_rmdir_bytes(path.as_bytes())
    }

    /// [`apply_rmdir`](Self::apply_rmdir) of a path given as bytes, never decoded.
    pub(crate) fn apply_rmdir_bytes(&self, path: &[u8]) -> Result<()> {
        self.refuse_write()?;
        let (parent, name) = self.resolve_parent(path)?;
        self.apply_rmdir_at(parent, name)
    }

    /// [`apply_rmdir`](Self::apply_rmdir) of entry `name` in directory
    /// `dir`. `.` is refused with `Error::InvalidArgument` and `..` with
    /// `Error::DirectoryNotEmpty`, as rmdir(2) does.
    pub fn apply_rmdir_at(&self, dir: impl Into<InodeRef>, name: &[u8]) -> Result<()> {
        self.refuse_write()?;
        check_entry_name(name)?;
        // `.` is the directory itself: removing it freed the directory
        // while its parent's entry still pointed at it.
        if name == b"." {
            return Err(Error::InvalidArgument("rmdir: cannot remove ."));
        }
        if name == b".." {
            return Err(Error::DirectoryNotEmpty);
        }
        let dir = dir.into();
        let parent_ino = dir.ino;
        let (parent_inode, mut parent_raw) = self.live_dir(dir)?;
        let target_ino = self.find_entry_in_dir(parent_ino, &parent_inode, name)?;
        let (target_inode, _) = self.live_inode(target_ino.into())?;
        if !target_inode.is_dir() {
            return Err(Error::NotADirectory);
        }

        // Empty-check: walk every block, reject if any entry is not "." or "..".
        let bs = self.sb.block_size();
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
        let blocks = target_inode.size.div_ceil(bs as u64);
        for logical in 0..blocks {
            // Either mapping: a block-mapped directory's i_block is not an
            // extent header.
            let Some(phys) = self.map_inode_logical(&target_inode, logical)? else {
                continue;
            };
            let block = self.read_block(phys)?;
            // The emptiness decision is made from this block's contents, and
            // the block is then freed. Unverified, a corrupt one reads as
            // empty or not-empty by accident.
            self.refuse_unverified_dir_block(target_ino, &target_inode, logical, &block)?;
            for entry in crate::dir::DirBlockIter::new(&block, has_ft) {
                let e = entry?;
                if e.name != b"." && e.name != b".." {
                    return Err(Error::DirectoryNotEmpty);
                }
            }
        }

        // Multi-block transaction: free target data blocks + free inode +
        // remove parent's dir entry + decrement parent nlink, all atomic.
        let mut buf = BlockBuffer::new(bs);

        // Free target's data blocks. Each freed run credits its own group's
        // BGD; SB credit accumulates and lands once below.
        let mut freed_blocks = if target_inode.has_extents() {
            let runs = self.extent_tree_runs(target_ino, &target_inode)?;
            self.buffer_free_runs(&mut buf, &runs)?
        } else {
            // A block-mapped directory: read as an extent header, it was
            // refused as a corrupt tree on a valid volume.
            self.buffer_free_block_map(&mut buf, &target_inode)?
        };

        if target_inode.file_acl != 0 {
            freed_blocks += self.buffer_release_xattr_block(&mut buf, target_inode.file_acl)?;
        }

        // Free the inode slot. A removed dir decrements `bg_used_dirs_count`
        // — buffer_free_inode_slot already credits free_inodes by +1, so we
        // separately patch used_dirs by -1 here.
        self.buffer_free_inode_slot(&mut buf, target_ino)?;
        let target_gi = ((target_ino - 1) / self.sb.inodes_per_group) as usize;
        self.buffer_patch_bgd_counters(&mut buf, target_gi, 0, 0, -1)?;
        // SB: free_blocks_count += freed, free_inodes_count += 1.
        self.buffer_patch_sb_counters(&mut buf, freed_blocks as i64, 1)?;

        // Zero the freed directory inode body (mode/links -> 0, set dtime, keep
        // the generation) so the slot no longer reads as a live directory.
        // Without this the freed inode keeps S_IFDIR + its "." / ".." and
        // e2fsck reports "unconnected directory inode", a stale ".." and bad
        // refcounts — the same cleanup apply_unlink already does for files.
        let inode_size = self.sb.inode_size as usize;
        let mut target_raw = vec![0u8; inode_size];
        let dtime = self.dtime_now();
        target_raw[0x14..0x18].copy_from_slice(&dtime.to_le_bytes());
        target_raw[0x64..0x68].copy_from_slice(&target_inode.generation.to_le_bytes());
        self.finalize_inode_raw(target_ino, target_inode.generation, &mut target_raw)?;
        self.buffer_write_inode(&mut buf, target_ino, &target_raw)?;

        // Remove the entry from the parent directory.
        let parent_blocks = parent_inode.size.div_ceil(bs as u64);
        let mut removed = false;
        for logical in 0..parent_blocks {
            let Some(phys) = self.map_inode_logical(&parent_inode, logical)? else {
                continue;
            };
            let block = buf.get_mut(self, phys)?;
            // An index block holds no entries to remove, and read as one it
            // ends in a tail-shaped dt_reserved and a `..` spanning the rest
            // (#233).
            if Self::is_htree_index_block(&parent_inode, logical, block) {
                continue;
            }
            let reserved_tail = if self.csum.enabled && crate::dir::has_csum_tail(block) {
                12
            } else {
                0
            };
            if crate::dir::remove_entry_from_block(block, name, has_ft, reserved_tail)? {
                if self.csum.enabled && reserved_tail == 12 {
                    self.csum
                        .patch_dir_entry_tail(parent_ino, parent_inode.generation, block);
                }
                removed = true;
                break;
            }
        }
        if !removed {
            return Err(Error::Corrupt(
                "apply_rmdir: entry disappeared mid-operation",
            ));
        }

        // Parent loses the ".." reference from the removed child → nlink -1.
        self.patch_inode_nlink(parent_ino, &mut parent_raw, &parent_inode, -1)?;
        self.buffer_write_inode(&mut buf, parent_ino, &parent_raw)?;

        self.commit_block_buffer(buf)
    }
}

/// `EXT4_LINK_MAX`: the most links the kernel gives one inode.
pub(crate) const EXT4_LINK_MAX: u16 = 65000;

/// The link count `count` becomes after `delta` links are added (positive)
/// or dropped (negative), one at a time, by the kernel's rules:
///
/// - `ext4_inc_count`: a count already at [`EXT4_LINK_MAX`] takes no more
///   links ([`Error::TooManyLinks`]) -- except a directory on a `DIR_NLINK`
///   volume, whose count is pinned at 1, "too many to count". A directory
///   already at 1 stays at 1.
/// - `ext4_dec_count`: a directory's count only drops while it is above 2,
///   so a pinned 1 is never taken to 0 (which Linux refuses to load); a
///   file's count stops at 0.
pub(crate) fn next_links_count(
    is_dir: bool,
    count: u16,
    delta: i32,
    dir_nlink: bool,
) -> Result<u16> {
    let mut count = count;
    for _ in 0..delta.unsigned_abs() {
        count = if delta > 0 {
            if is_dir && count == 1 {
                1
            } else if count >= EXT4_LINK_MAX {
                if is_dir && dir_nlink {
                    1
                } else {
                    return Err(Error::TooManyLinks);
                }
            } else {
                count + 1
            }
        } else if is_dir {
            if count > 2 {
                count - 1
            } else {
                count
            }
        } else {
            count.saturating_sub(1)
        };
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::inode::{
        EXTRA_ISIZE_DEFAULT, INODE_SIZE_WITH_CRTIME, INODE_SIZE_WITH_EXTRA, OFF_ATIME, OFF_CRTIME,
        OFF_CTIME, OFF_EXTRA_ISIZE, OFF_GENERATION, OFF_MTIME,
    };

    fn read_le32(buf: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
    }
    fn read_le16(buf: &[u8], off: usize) -> u16 {
        u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
    }

    // --- write_inode_timestamps ---

    #[test]
    fn write_inode_timestamps_sets_atime_ctime_mtime() {
        let mut raw = vec![0u8; 256];
        write_inode_timestamps(&mut raw, 0x5EAD_BEEF);
        assert_eq!(read_le32(&raw, OFF_ATIME), 0x5EAD_BEEF);
        assert_eq!(read_le32(&raw, OFF_CTIME), 0x5EAD_BEEF);
        assert_eq!(read_le32(&raw, OFF_MTIME), 0x5EAD_BEEF);
    }

    /// With no `*_extra` fields (i_extra_isize 0) a time past 2038 is
    /// clamped, as the kernel does, not wrapped to a negative base.
    #[test]
    fn write_inode_timestamps_clamps_past_2038_without_extra_fields() {
        let mut raw = vec![0u8; 256];
        write_inode_timestamps(&mut raw, 0xDEAD_BEEF);
        assert_eq!(read_le32(&raw, OFF_ATIME), i32::MAX as u32);
        assert_eq!(read_le32(&raw, OFF_CTIME), i32::MAX as u32);
        assert_eq!(read_le32(&raw, OFF_MTIME), i32::MAX as u32);
    }

    /// With them, the same time keeps its epoch bits and reads back whole.
    #[test]
    fn write_inode_timestamps_keeps_epoch_bits_with_extra_fields() {
        let mut raw = vec![0u8; 256];
        write_inode_extra_isize(&mut raw);
        write_inode_timestamps(&mut raw, 0xDEAD_BEEF);
        let inode = Inode::parse(&raw).unwrap();
        assert_eq!(inode.atime, 0xDEAD_BEEF);
        assert_eq!(inode.ctime, 0xDEAD_BEEF);
        assert_eq!(inode.mtime, 0xDEAD_BEEF);
        assert_eq!(inode.crtime, 0xDEAD_BEEF);
    }

    #[test]
    fn write_inode_timestamps_sets_crtime_when_large_enough() {
        let mut raw = vec![0u8; INODE_SIZE_WITH_CRTIME + 4];
        write_inode_extra_isize(&mut raw);
        write_inode_timestamps(&mut raw, 0x1234_5678);
        assert_eq!(read_le32(&raw, OFF_CRTIME), 0x1234_5678);
    }

    #[test]
    fn write_inode_timestamps_skips_crtime_when_too_small() {
        let mut raw = vec![0xAAu8; INODE_SIZE_WITH_CRTIME - 1];
        write_inode_timestamps(&mut raw, 0x1234_5678);
        // Buffer too small for crtime — no write, no panic.
        // atime/ctime/mtime still set.
        assert_eq!(read_le32(&raw, OFF_ATIME), 0x1234_5678);
    }

    #[test]
    fn write_inode_timestamps_zero_now() {
        let mut raw = vec![0xFFu8; 256];
        write_inode_timestamps(&mut raw, 0);
        assert_eq!(read_le32(&raw, OFF_ATIME), 0);
        assert_eq!(read_le32(&raw, OFF_CTIME), 0);
        assert_eq!(read_le32(&raw, OFF_MTIME), 0);
        assert_eq!(read_le32(&raw, OFF_CRTIME), 0);
    }

    // --- write_inode_generation ---

    #[test]
    fn write_inode_generation_writes_at_correct_offset() {
        let mut raw = vec![0u8; 256];
        write_inode_generation(&mut raw, 0xCAFE_BABE);
        assert_eq!(read_le32(&raw, OFF_GENERATION), 0xCAFE_BABE);
    }

    #[test]
    fn write_inode_generation_overwrites_existing() {
        let mut raw = vec![0xFFu8; 256];
        write_inode_generation(&mut raw, 0);
        assert_eq!(read_le32(&raw, OFF_GENERATION), 0);
    }

    // --- write_inode_extra_isize ---

    #[test]
    fn write_inode_extra_isize_sets_default_when_large_enough() {
        let mut raw = vec![0u8; INODE_SIZE_WITH_EXTRA + 4];
        write_inode_extra_isize(&mut raw);
        assert_eq!(read_le16(&raw, OFF_EXTRA_ISIZE), EXTRA_ISIZE_DEFAULT);
    }

    #[test]
    fn write_inode_extra_isize_skips_when_too_small() {
        let mut raw = vec![0u8; INODE_SIZE_WITH_EXTRA - 1];
        write_inode_extra_isize(&mut raw); // must not panic
                                           // No bytes should have been written — buffer too small.
    }

    // --- alloc_inode_generation ---

    #[test]
    fn alloc_inode_generation_produces_unique_values() {
        let g1 = crate::runtime::Runtime::next_inode_generation(&crate::runtime::SystemRuntime);
        let g2 = crate::runtime::Runtime::next_inode_generation(&crate::runtime::SystemRuntime);
        assert_ne!(g1, g2, "successive calls must produce distinct values");
    }

    // ---------------------------------------------------------------
    // Orphan recovery: the two kinds of orphan
    // ---------------------------------------------------------------
    //
    // The kernel puts an inode on the `s_last_orphan` chain for two
    // different reasons, and `ext4_orphan_cleanup` branches on
    // `i_links_count` to tell them apart. Zero means unlinked-while-open:
    // really delete it. Non-zero means a `truncate()` that a crash
    // interrupted: the file is still named by its directory entries, and
    // recovery is supposed to finish the truncate and leave it in place.
    //
    // These tests build both shapes on a formatted in-memory volume and
    // pin the two outcomes against each other.

    struct MemDev {
        bytes: std::sync::Mutex<Vec<u8>>,
        size: u64,
    }

    impl MemDev {
        fn new(size: u64) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                bytes: std::sync::Mutex::new(vec![0u8; size as usize]),
                size,
            })
        }
    }

    impl crate::block_io::BlockDevice for MemDev {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let b = self.bytes.lock().unwrap();
            let start = offset as usize;
            let end = start + buf.len();
            if end > b.len() {
                return Err(Error::Corrupt("MemDev: read past end"));
            }
            buf.copy_from_slice(&b[start..end]);
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            self.size
        }
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
            let mut b = self.bytes.lock().unwrap();
            let start = offset as usize;
            let end = start + buf.len();
            if end > b.len() {
                return Err(Error::Corrupt("MemDev: write past end"));
            }
            b[start..end].copy_from_slice(buf);
            Ok(())
        }
        fn flush(&self) -> Result<()> {
            Ok(())
        }
        fn is_writable(&self) -> bool {
            true
        }
    }

    const BS: u32 = 4096;
    const VOL: u64 = 32 * 1024 * 1024;

    fn formatted() -> std::sync::Arc<MemDev> {
        let dev = MemDev::new(VOL);
        crate::mkfs::format_filesystem(dev.as_ref(), Some("orphan"), None, VOL, BS)
            .expect("format");
        dev
    }

    fn mount(dev: &std::sync::Arc<MemDev>) -> Filesystem {
        Filesystem::mount(dev.clone()).expect("mount")
    }

    /// `/f` holding `len` bytes of 0xAA; returns its inode number.
    fn file_of_aa(fs: &Filesystem, len: usize) -> u32 {
        let ino = fs.apply_create("/f", 0o644).unwrap();
        fs.apply_replace_file_content("/f", &vec![0xAAu8; len])
            .unwrap();
        ino
    }

    /// The file's bytes, and how many of them disagree with "0xAA outside
    /// `zeroed`, 0 inside it".
    fn bytes_wrong_after_zeroing(
        fs: &Filesystem,
        ino: u32,
        zeroed: std::ops::Range<usize>,
    ) -> usize {
        let (inode, _) = fs.read_inode_verified(ino).unwrap();
        let data = crate::file_io::read_all(fs, &inode).unwrap();
        data.iter()
            .enumerate()
            .filter(|(i, b)| **b != if zeroed.contains(i) { 0 } else { 0xAA })
            .count()
    }

    /// An unaligned punch keeps the bytes outside its range (#388): the
    /// edge blocks are zeroed in part, not freed whole.
    #[test]
    fn an_unaligned_punch_keeps_the_bytes_around_it() {
        let dev = formatted();
        let fs = mount(&dev);
        let ino = file_of_aa(&fs, 8192);
        fs.apply_fallocate_punch_hole(ino, 100, 100).unwrap();
        assert_eq!(bytes_wrong_after_zeroing(&fs, ino, 100..200), 0);
        drop(fs);
        let fs = mount(&dev);
        assert_eq!(
            bytes_wrong_after_zeroing(&fs, ino, 100..200),
            0,
            "after a remount"
        );
    }

    /// A punch spanning whole blocks frees those and zeroes the partial
    /// ones at each end in place (#388).
    #[test]
    fn a_punch_frees_the_whole_blocks_and_zeroes_the_edges() {
        let dev = formatted();
        let fs = mount(&dev);
        let ino = file_of_aa(&fs, 3 * BS as usize);
        drop(fs);
        let fs = mount(&dev); // `fs.sb` is the superblock as mounted
        let free_before = fs.sb.free_blocks_count;
        let (lo, hi) = (100usize, 2 * BS as usize + 100);
        fs.apply_fallocate_punch_hole(ino, lo as u64, (hi - lo) as u64)
            .unwrap();
        drop(fs);
        let fs = mount(&dev);
        assert_eq!(bytes_wrong_after_zeroing(&fs, ino, lo..hi), 0);
        let (inode, _) = fs.read_inode_verified(ino).unwrap();
        let mapped: Vec<bool> = (0..3)
            .map(|lb| fs.map_inode_logical(&inode, lb).unwrap().is_some())
            .collect();
        assert_eq!(
            mapped,
            [true, false, true],
            "only the middle block is freed"
        );
        assert_eq!(fs.sb.free_blocks_count, free_before + 1, "one block freed");
    }

    /// A punch inside one block frees nothing.
    #[test]
    fn a_punch_inside_one_block_frees_nothing() {
        let dev = formatted();
        let fs = mount(&dev);
        let ino = file_of_aa(&fs, 2 * BS as usize);
        drop(fs);
        let fs = mount(&dev); // `fs.sb` is the superblock as mounted
        let free_before = fs.sb.free_blocks_count;
        fs.apply_fallocate_punch_hole(ino, BS as u64 + 1, BS as u64 - 2)
            .unwrap();
        drop(fs);
        let fs = mount(&dev);
        let (lo, hi) = (BS as usize + 1, 2 * BS as usize - 1);
        assert_eq!(bytes_wrong_after_zeroing(&fs, ino, lo..hi), 0);
        assert_eq!(fs.sb.free_blocks_count, free_before);
    }

    /// An unaligned zero-range zeroes exactly its range (#388): it is a
    /// punch plus a preallocation, and inherited the punch's rounding.
    #[test]
    fn an_unaligned_zero_range_keeps_the_bytes_around_it() {
        let dev = formatted();
        let fs = mount(&dev);
        let ino = file_of_aa(&fs, 3 * BS as usize);
        let (lo, hi) = (100usize, 2 * BS as usize + 100);
        fs.apply_fallocate_zero_range(ino, lo as u64, (hi - lo) as u64)
            .unwrap();
        drop(fs);
        let fs = mount(&dev);
        assert_eq!(bytes_wrong_after_zeroing(&fs, ino, lo..hi), 0);
    }

    /// `/f` with one block written at every other logical block from 0 to
    /// 10: six extents, so its tree is depth 1. Returns its inode number.
    fn striped_file(fs: &Filesystem) -> u32 {
        let ino = fs.apply_create("/f", 0o644).unwrap();
        for lb in [0u64, 2, 4, 6, 8, 10] {
            fs.apply_pwrite("/f", lb * BS as u64, &[1u8; BS as usize])
                .unwrap();
        }
        let (inode, _) = fs.read_inode_verified(ino).unwrap();
        let depth = u16::from_le_bytes(inode.block[6..8].try_into().unwrap());
        assert!(depth >= 1, "precondition: depth >= 1, got {depth}");
        ino
    }

    /// Fill the volume with `/fill`, leaving no more than a few blocks free.
    fn fill_leaving_a_few_free(fs: &Filesystem) {
        let mut n = fs.sb.free_blocks_count.saturating_sub(4);
        loop {
            let r = fs.apply_create("/fill", 0o644).and_then(|_| {
                fs.apply_replace_file_content("/fill", &vec![0u8; (n * BS as u64) as usize])
            });
            match r {
                Ok(_) => break,
                Err(_) => {
                    let _ = fs.apply_unlink("/fill");
                    n -= 1;
                }
            }
        }
    }

    /// A pwrite that fails with ENOSPC part-way leaves no extent on disk
    /// that maps blocks the bitmap still calls free (#389): the tree nodes
    /// it rewrote were written straight to the device, ahead of the
    /// transaction that never committed.
    #[test]
    fn a_pwrite_that_runs_out_of_space_leaves_the_tree_as_it_was() {
        let dev = formatted();
        let fs = mount(&dev);
        let ino = striped_file(&fs);
        drop(fs);
        let fs = mount(&dev);
        fill_leaving_a_few_free(&fs);
        drop(fs);
        let fs = mount(&dev);
        let left = fs.sb.free_blocks_count;
        let bs = BS as u64;
        let r = fs.apply_pwrite("/f", 20 * bs, &vec![2u8; ((left + 8) * bs) as usize]);
        assert!(r.is_err(), "precondition: ENOSPC ({left} free)");
        drop(fs);
        let fs = mount(&dev);
        let (inode, _) = fs.read_inode_verified(ino).unwrap();
        let mapped = fs.map_inode_logical(&inode, 20).unwrap();
        assert_eq!(
            mapped, None,
            "a failed pwrite left logical block 20 mapped ({left} were free)"
        );
        let data = crate::file_io::read_all(&fs, &inode).unwrap();
        assert_eq!(data.len(), 11 * BS as usize, "the size is unchanged");
    }

    /// Two plans in one open transaction must not hand out the same blocks
    /// of a BLOCK_UNINIT group. The first plan's staging clears the group's
    /// flag only on the buffer, so a second plan that still sees the flag
    /// re-synthesises the bitmap from metadata alone and ignores the staged
    /// bits -- a data run's second sub-allocation, or an extent-tree block,
    /// then lands on top of the first.
    /// A formatted volume whose group 0 is flagged BLOCK_UNINIT. Only the
    /// planners are run on it: a plan synthesises the group's bitmap from its
    /// metadata, which is what the flag says to do.
    fn formatted_with_group_zero_block_uninit() -> std::sync::Arc<MemDev> {
        let dev = formatted();
        let fs = mount(&dev);
        let (bgt_block, off) = fs.sb.descriptor_location(0);
        let ds = fs.sb.desc_size as usize;
        let mut raw = fs.read_block(bgt_block).unwrap();
        let flags = u16::from_le_bytes(raw[off + 0x12..off + 0x14].try_into().unwrap())
            | crate::bgd::BgdFlags::BLOCK_UNINIT.bits();
        raw[off + 0x12..off + 0x14].copy_from_slice(&flags.to_le_bytes());
        let c = crate::checksum::group_desc_csum(&fs.sb, &fs.csum, 0, &raw[off..off + ds])
            .expect("the formatted volume checksums its descriptors");
        raw[off + 0x1e..off + 0x20].copy_from_slice(&c.to_le_bytes());
        dev.write_at(bgt_block * u64::from(BS), &raw).unwrap();
        drop(fs);
        dev
    }

    #[test]
    fn buffered_allocations_do_not_reuse_an_uninitialized_groups_first_run() {
        let dev = formatted_with_group_zero_block_uninit();
        let fs = mount(&dev);
        assert!(fs.allocation_groups()[0]
            .flags()
            .contains(crate::bgd::BgdFlags::BLOCK_UNINIT));
        let before = dev.bytes.lock().unwrap().clone();

        let mut buf = BlockBuffer::new(BS);
        let first = fs.plan_buffered_block_allocation(&buf, 4, 0).unwrap();
        fs.buffer_mark_block_run_used(&mut buf, first.first_block, 4)
            .unwrap();
        let next = fs.plan_buffered_block_allocation(&buf, 1, 0).unwrap();
        assert!(
            next.first_block < first.first_block || next.first_block >= first.first_block + 4,
            "second plan {} overlaps the staged run {}..{}",
            next.first_block,
            first.first_block,
            first.first_block + 4
        );

        // Nothing committed: the mount still sees the group as uninit, the
        // device is untouched, and a fresh transaction plans as before.
        assert!(fs.allocation_groups()[0]
            .flags()
            .contains(crate::bgd::BgdFlags::BLOCK_UNINIT));
        assert!(
            *dev.bytes.lock().unwrap() == before,
            "planning wrote to the device"
        );
        drop(buf);
        let again = fs
            .plan_buffered_block_allocation(&BlockBuffer::new(BS), 4, 0)
            .unwrap();
        assert_eq!(again.first_block, first.first_block);
    }

    /// The one-block allocator a punch's tree repack and an xattr unshare
    /// draw from must see the uninit clear its own first call staged (#291).
    /// A punch that splits an extent in a tree of full leaves calls it twice
    /// in one transaction, for a new leaf and an index node above it; the
    /// second call planned without the buffer's pending clear, rebuilt the
    /// group's bitmap from metadata and returned the leaf's block again.
    #[test]
    fn one_block_allocations_in_a_transaction_do_not_reuse_an_uninitialized_groups_block() {
        let dev = formatted_with_group_zero_block_uninit();
        let fs = mount(&dev);
        let mut buf = BlockBuffer::new(BS);
        let first = fs.buffer_allocate_block(&mut buf, 1).unwrap();
        let second = fs.buffer_allocate_block(&mut buf, 1).unwrap();
        assert_ne!(
            first, second,
            "the second allocation handed out block {first} again"
        );
    }

    /// Set `flag` in group `gi`'s on-disk descriptor, restamping its
    /// checksum, behind the back of any mount.
    fn set_group_flag(dev: &std::sync::Arc<MemDev>, gi: u64, flag: crate::bgd::BgdFlags) {
        let fs = mount(dev);
        let bs = u64::from(fs.sb.block_size());
        let (bgt_block, off) = fs.sb.descriptor_location(gi);
        let ds = fs.sb.desc_size as usize;
        let mut raw = fs.read_block(bgt_block).unwrap();
        let flags =
            u16::from_le_bytes(raw[off + 0x12..off + 0x14].try_into().unwrap()) | flag.bits();
        raw[off + 0x12..off + 0x14].copy_from_slice(&flags.to_le_bytes());
        if let Some(c) =
            crate::checksum::group_desc_csum(&fs.sb, &fs.csum, gi as u32, &raw[off..off + ds])
        {
            raw[off + 0x1e..off + 0x20].copy_from_slice(&c.to_le_bytes());
        }
        drop(fs);
        dev.write_at(bgt_block * bs, &raw).unwrap();
    }

    fn block_uninit(fs: &Filesystem, gi: usize) -> bool {
        fs.allocation_groups()[gi]
            .flags()
            .contains(crate::bgd::BgdFlags::BLOCK_UNINIT)
    }

    /// Clear group `gi`'s BLOCK_UNINIT in a transaction of its own and
    /// commit it, which is what publishes the change to the mount.
    fn clear_and_publish(fs: &Filesystem, gi: usize) {
        let mut buf = BlockBuffer::new(fs.sb.block_size());
        assert!(fs
            .clear_bgd_uninit_flag_if_set(&mut buf, gi, BgdUninitFlag::Block)
            .unwrap());
        // No counter moves; this is what restamps the descriptor checksum.
        fs.buffer_patch_bgd_counters(&mut buf, gi, 0, 0, 0).unwrap();
        fs.commit_block_buffer(buf).unwrap();
    }

    /// The descriptors the planners are handed are cached once any uninit
    /// flag has been cleared, rather than cloned on every call (#333). A
    /// cache read before a transaction publishes must not survive it: the
    /// planner would go on treating the group the transaction woke as
    /// untouched, and hand out its blocks again without reading its bitmap.
    #[test]
    fn a_published_uninit_clear_is_seen_through_the_cached_descriptors() {
        // 2 KiB blocks put 16,384 in a group, so 128 MiB is four groups:
        // more than one to wake.
        const MULTI: u64 = 128 * 1024 * 1024;
        let dev = MemDev::new(MULTI);
        crate::mkfs::format_filesystem(dev.as_ref(), Some("cache"), None, MULTI, 2048)
            .expect("format");
        for gi in [1, 2, 3] {
            set_group_flag(&dev, gi, crate::bgd::BgdFlags::BLOCK_UNINIT);
        }
        let mut fs = mount(&dev);
        assert!(fs.groups.len() >= 4, "{} groups", fs.groups.len());
        assert!(block_uninit(&fs, 1) && block_uninit(&fs, 2) && block_uninit(&fs, 3));

        // First publish: from here on the descriptors come from the cache,
        // and this read fills it.
        clear_and_publish(&fs, 1);
        assert!(!block_uninit(&fs, 1));
        assert!(block_uninit(&fs, 2));

        // Second publish, with the cache full.
        clear_and_publish(&fs, 2);
        assert!(!block_uninit(&fs, 1), "an earlier clear was lost");
        assert!(
            !block_uninit(&fs, 2),
            "a transaction published and the planners still see the group it woke as uninit"
        );
        assert!(block_uninit(&fs, 3));

        // Re-reading the descriptors (`fresh_read`) resets what the mount
        // has cleared, and must drop the cache with it: the next publish
        // builds on the descriptors as read now.
        fs.fresh_read().unwrap();
        assert!(!block_uninit(&fs, 1) && !block_uninit(&fs, 2) && block_uninit(&fs, 3));
        clear_and_publish(&fs, 3);
        assert!(!block_uninit(&fs, 1) && !block_uninit(&fs, 2) && !block_uninit(&fs, 3));
    }

    /// A directory block is one block. A plan for any other count is refused
    /// whole, rather than marking one block of it and applying the plan's
    /// counter deltas for all of them (#333).
    #[test]
    fn a_dir_block_commit_refuses_a_plan_for_more_than_one_block() {
        let dev = formatted();
        let fs = mount(&dev);
        let plan = fs
            .plan_buffered_block_allocation(&BlockBuffer::new(BS), 2, 0)
            .unwrap();
        assert_eq!(plan.count, 2);
        let before = dev.bytes.lock().unwrap().clone();
        let mut buf = BlockBuffer::new(BS);
        let got = fs.buffer_dir_block_alloc(&mut buf, &plan);
        assert!(
            matches!(got, Err(Error::Corrupt(_))),
            "a count-2 plan was staged as a directory block: {got:?}"
        );
        assert!(
            buf.dirty.is_empty() && buf.uninit_cleared.is_empty(),
            "the refused plan staged {} blocks",
            buf.dirty.len()
        );
        assert!(
            *dev.bytes.lock().unwrap() == before,
            "the refused plan wrote to the device"
        );
    }

    /// #381: set_flags must not let a caller set INDEX_FL on a linear
    /// directory -- the driver then reads block 0 as a dx_root.
    #[test]
    fn set_flags_refuses_index_on_a_linear_directory() {
        let dev = formatted();
        let fs = mount(&dev);
        let d = fs.apply_mkdir("/d", 0o755).unwrap();
        let (i, _) = fs.read_inode_verified(d).unwrap();
        let r = fs.apply_set_flags("/d", i.flags | crate::inode::InodeFlags::INDEX.bits());
        assert!(r.is_err(), "INDEX_FL accepted on a linear directory");
        let (after, _) = fs.read_inode_verified(d).unwrap();
        assert_eq!(after.flags, i.flags, "a refused set_flags changed i_flags");
    }

    /// #381: every bit outside the kernel's user-modifiable mask that
    /// changes how the inode's existing bytes are read is refused, set or
    /// cleared; the ones a caller may change still go through.
    #[test]
    fn set_flags_changes_only_user_modifiable_bits() {
        let dev = formatted();
        let fs = mount(&dev);
        let f = fs.apply_create("/f", 0o644).unwrap();
        let (i, _) = fs.read_inode_verified(f).unwrap();
        for (bit, name) in [
            (0x0000_0800u32, "ENCRYPT"),
            (0x0000_1000, "INDEX"),
            (0x0004_0000, "HUGE_FILE"),
            (0x0010_0000, "VERITY"),
            (0x0200_0000, "DAX"),
            (0x4000_0000, "CASEFOLD"),
            (0x8000_0000, "RESERVED"),
        ] {
            assert!(
                fs.apply_set_flags("/f", i.flags | bit).is_err(),
                "{name} ({bit:#x}) accepted"
            );
        }
        // A bit already set that the caller cannot change passes unchanged.
        let wanted = i.flags
            | crate::inode::InodeFlags::IMMUTABLE.bits()
            | crate::inode::InodeFlags::NOATIME.bits()
            | crate::inode::InodeFlags::NODUMP.bits();
        fs.apply_set_flags("/f", wanted).unwrap();
        let (after, _) = fs.read_inode_verified(f).unwrap();
        assert_eq!(after.flags, wanted);
        fs.apply_set_flags("/f", i.flags).unwrap();
        let (after, _) = fs.read_inode_verified(f).unwrap();
        assert_eq!(after.flags, i.flags);
    }

    /// `Filesystem::mount_recovering` / `finish` on a journalled volume.
    mod checked_recovery {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};

        fn journalled() -> std::sync::Arc<MemDev> {
            let dev = MemDev::new(VOL);
            crate::mkfs::format_filesystem_with_flavor(
                dev.as_ref(),
                Some("checked"),
                None,
                VOL,
                BS,
                features::FsFlavor::Ext3,
            )
            .expect("format");
            dev
        }

        fn on_disk(dev: &MemDev) -> Superblock {
            Superblock::read(dev).expect("superblock")
        }

        fn needs_recovery(dev: &MemDev) -> bool {
            on_disk(dev).feature_incompat & features::Incompat::RECOVER.bits() != 0
        }

        /// The marker is the lifecycle's promise: while the handle is
        /// owned, the next owner must treat the volume as in recovery.
        #[test]
        fn needs_recovery_stays_set_across_commits_until_finish() {
            let dev = journalled();
            let fs = Filesystem::mount_recovering(dev.clone()).expect("checked mount");
            fs.apply_create("/a", 0o644).expect("create");
            assert!(
                needs_recovery(&dev),
                "a commit on a checked mount cleared needs_recovery before finish"
            );
            fs.finish().expect("finish");
            assert!(!needs_recovery(&dev), "finish left needs_recovery set");
        }

        /// A checked mount marks the volume not clean on the way in; its
        /// release must put back the state it found, not the state it made.
        #[test]
        fn finish_puts_back_the_clean_state_it_found() {
            let dev = journalled();
            assert_ne!(on_disk(&dev).state & crate::superblock::EXT4_VALID_FS, 0);
            let fs = Filesystem::mount_recovering(dev.clone()).expect("checked mount");
            fs.apply_create("/a", 0o644).expect("create");
            fs.finish().expect("finish");
            assert_ne!(
                on_disk(&dev).state & crate::superblock::EXT4_VALID_FS,
                0,
                "a finished checked mount left the volume marked not clean"
            );
        }

        /// A MemDev whose every write and flush after `fail_at` fails.
        struct Failing {
            inner: std::sync::Arc<MemDev>,
            events: AtomicUsize,
            fail_at: AtomicUsize,
        }

        impl Failing {
            fn gate(&self) -> Result<()> {
                if self.events.fetch_add(1, Ordering::SeqCst) >= self.fail_at.load(Ordering::SeqCst)
                {
                    return Err(Error::Corrupt("injected I/O failure"));
                }
                Ok(())
            }
        }

        impl crate::block_io::BlockDevice for Failing {
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
                self.inner.read_at(offset, buf)
            }
            fn size_bytes(&self) -> u64 {
                self.inner.size_bytes()
            }
            fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
                self.gate()?;
                self.inner.write_at(offset, buf)
            }
            fn flush(&self) -> Result<()> {
                self.gate()
            }
            fn is_writable(&self) -> bool {
                true
            }
        }

        /// Every device write and flush of a checked mount, create and
        /// finish, the drop included, is one `finish` answers for: a failure
        /// at any of them must not be reported as a clean release.
        #[test]
        fn a_failure_at_any_release_write_is_not_a_clean_finish() {
            let run = |fail_at: usize| {
                let dev = std::sync::Arc::new(Failing {
                    inner: journalled(),
                    events: AtomicUsize::new(0),
                    fail_at: AtomicUsize::new(fail_at),
                });
                let finished = Filesystem::mount_recovering(dev.clone())
                    .and_then(|fs| fs.apply_create("/a", 0o644).map(|_| fs))
                    .and_then(|fs| fs.finish());
                (finished.is_ok(), dev.events.load(Ordering::SeqCst))
            };
            let (ok, events) = run(usize::MAX);
            assert!(ok, "the uninterrupted run must finish");
            for fail_at in 0..events {
                assert!(
                    !run(fail_at).0,
                    "a failure at device event {fail_at} of {events} was reported as a clean finish"
                );
            }
        }

        /// Once a journal commit failed mid-write, neither `finish` nor the
        /// drop that follows it may touch the device again.
        #[test]
        fn a_failed_commit_leaves_finish_and_drop_without_device_io() {
            let dev = std::sync::Arc::new(Failing {
                inner: journalled(),
                events: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(usize::MAX),
            });
            let fs = Filesystem::mount_recovering(dev.clone()).expect("checked mount");
            let first = dev.events.load(Ordering::SeqCst) + 2;
            dev.fail_at.store(first, Ordering::SeqCst);
            assert!(
                fs.apply_create("/a", 0o644).is_err(),
                "the commit must fail"
            );
            let after_failure = dev.events.load(Ordering::SeqCst);
            assert!(
                fs.finish().is_err(),
                "a failed journal claimed a clean finish"
            );
            assert_eq!(
                dev.events.load(Ordering::SeqCst),
                after_failure,
                "finish or drop wrote to the device after the journal failed"
            );
        }
    }

    fn resolve(fs: &Filesystem, path: &str) -> Result<u32> {
        let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
        crate::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, path)
    }

    /// Put `ino` on the orphan chain with the given link count and size.
    ///
    /// `links == 0` is the unlinked-while-open shape. A non-zero `links`
    /// with a lowered `size` is the shape a crash during `truncate()`
    /// leaves: `i_size` already reduced, the extents still covering the
    /// old range, the inode still named by its directory entries.
    fn plant_orphan(fs: &Filesystem, ino: u32, links: u16, new_size: Option<u64>) {
        let mut buf = BlockBuffer::new(fs.sb.block_size());
        let (inode, mut raw) = fs.read_inode_verified(ino).expect("read inode");
        if let Some(size) = new_size {
            Filesystem::patch_inode_size_and_blocks(&mut raw, size, inode.blocks)
                .expect("patch size");
        }
        raw[0x1A..0x1C].copy_from_slice(&links.to_le_bytes());
        // dtime doubles as the "next orphan" link; zero terminates.
        raw[0x14..0x18].copy_from_slice(&0u32.to_le_bytes());
        fs.finalize_inode_raw(ino, inode.generation, &mut raw)
            .expect("finalize");
        fs.buffer_write_inode(&mut buf, ino, &raw)
            .expect("write inode");
        fs.buffer_patch_sb_last_orphan(&mut buf, ino)
            .expect("patch s_last_orphan");
        fs.commit_block_buffer(buf).expect("commit");
    }

    /// Give `ino` an external xattr block the way a security label that does
    /// not fit in the inode would, counted in `i_blocks`. Returns the block.
    fn give_xattr_block(fs: &Filesystem, ino: u32) -> u64 {
        let mut buf = BlockBuffer::new(BS);
        let xb = fs.buffer_allocate_block(&mut buf, ino).unwrap();
        let mut blk = vec![0u8; BS as usize];
        crate::xattr::plan_set_in_external_block(&mut blk, "security.selinux", &[b'z'; 32], 1)
            .unwrap();
        fs.csum.patch_xattr_block(xb, &mut blk);
        buf.put(xb, blk);
        let (inode, mut raw) = fs.read_inode_verified(ino).unwrap();
        Filesystem::write_file_acl(&mut raw, xb).unwrap();
        Filesystem::patch_inode_size_and_blocks(&mut raw, inode.size, inode.blocks + 8).unwrap();
        fs.finalize_inode_raw(ino, inode.generation, &mut raw)
            .unwrap();
        fs.buffer_write_inode(&mut buf, ino, &raw).unwrap();
        fs.commit_block_buffer(buf).unwrap();
        xb
    }

    /// Whether `block` is marked in use in its group's bitmap.
    fn block_bit_set(fs: &Filesystem, block: u64) -> bool {
        let rel = block - fs.sb.first_data_block as u64;
        let gi = (rel / fs.sb.blocks_per_group as u64) as usize;
        let bit = (rel % fs.sb.blocks_per_group as u64) as usize;
        let bm = fs.read_block(fs.groups[gi].block_bitmap).unwrap();
        bm[bit / 8] & (1 << (bit % 8)) != 0
    }

    /// #384: orphan recovery must not read a fast symlink's target as a
    /// block map because it also holds an xattr block. The member is still
    /// reclaimed, its xattr block with it.
    #[test]
    fn recovering_an_orphaned_fast_symlink_frees_no_block_named_by_its_target() {
        let dev = formatted();
        let fs = mount(&dev);
        let (root, _) = fs.read_inode_verified(2).unwrap();
        let victim = fs.map_inode_logical(&root, 0).unwrap().unwrap();
        let mut tb: Vec<u8> = (victim as u32).to_le_bytes().to_vec();
        while tb.last() == Some(&0) {
            tb.pop();
        }
        assert!(
            tb.iter().all(|&b| b > 0 && b < 0x80),
            "block {victim} not expressible as ASCII"
        );
        let t = String::from_utf8(tb).unwrap();
        let ino = fs.apply_symlink(&t, "/l").unwrap();
        let xb = give_xattr_block(&fs, ino);
        plant_orphan(&fs, ino, 0, None);
        drop(fs);
        let fs = mount(&dev); // recovers orphans
        assert!(
            block_bit_set(&fs, victim),
            "block {victim} (the root directory's) was freed by orphan recovery"
        );
        assert!(!block_bit_set(&fs, xb), "the xattr block {xb} was kept");
        assert_eq!(fs.sb.last_orphan, 0, "the orphan chain was not drained");
    }

    /// #384: a fast symlink with an xattr block whose target, read as block
    /// pointers, lies outside the volume is still reclaimed. It used to be
    /// left on the chain, and every orphan behind it with it.
    #[test]
    fn an_orphaned_fast_symlink_whose_target_is_no_block_does_not_stall_the_chain() {
        let dev = formatted();
        let fs = mount(&dev);
        let behind = fs.apply_create("/behind", 0o644).unwrap();
        plant_orphan(&fs, behind, 0, None);
        let ino = fs.apply_symlink("zzzz", "/l").unwrap();
        let xb = give_xattr_block(&fs, ino);
        // The symlink heads the chain; `behind` follows it.
        let (inode, mut raw) = fs.read_inode_verified(ino).unwrap();
        raw[0x1A..0x1C].copy_from_slice(&0u16.to_le_bytes());
        raw[0x14..0x18].copy_from_slice(&behind.to_le_bytes());
        fs.finalize_inode_raw(ino, inode.generation, &mut raw)
            .unwrap();
        let mut buf = BlockBuffer::new(BS);
        fs.buffer_write_inode(&mut buf, ino, &raw).unwrap();
        fs.buffer_patch_sb_last_orphan(&mut buf, ino).unwrap();
        fs.commit_block_buffer(buf).unwrap();
        drop(fs);
        let fs = mount(&dev); // recovers orphans
        assert_eq!(fs.sb.last_orphan, 0, "the orphan chain was not drained");
        assert!(!block_bit_set(&fs, xb), "the xattr block {xb} was kept");
        for gone in [ino, behind] {
            let (i, _) = fs.read_inode_verified(gone).unwrap();
            assert_eq!(i.mode, 0, "orphan {gone} was not reclaimed");
        }
    }

    /// #384: the same for an inline-data file holding an xattr block: its
    /// data bytes are not block pointers.
    #[test]
    fn recovering_an_orphaned_inline_file_frees_no_block_named_by_its_data() {
        let dev = formatted();
        set_incompat_bit(&dev, crate::features::Incompat::INLINE_DATA.bits());
        let fs = mount(&dev);
        let (root, _) = fs.read_inode_verified(2).unwrap();
        let victim = fs.map_inode_logical(&root, 0).unwrap().unwrap();
        let ino = fs.apply_create("/f", 0o644).unwrap();
        let (inode, mut raw) = fs.read_inode_verified(ino).unwrap();
        let mut flags = u32::from_le_bytes(raw[0x20..0x24].try_into().unwrap());
        flags &= !crate::inode::InodeFlags::EXTENTS.bits();
        flags |= crate::inode::InodeFlags::INLINE_DATA.bits();
        raw[0x20..0x24].copy_from_slice(&flags.to_le_bytes());
        raw[0x28..0x64].fill(0);
        raw[0x28..0x2C].copy_from_slice(&(victim as u32).to_le_bytes());
        raw[0x04..0x08].copy_from_slice(&4u32.to_le_bytes());
        fs.finalize_inode_raw(ino, inode.generation, &mut raw)
            .unwrap();
        fs.write_inode_raw(ino, &raw).unwrap();
        let xb = give_xattr_block(&fs, ino);
        plant_orphan(&fs, ino, 0, None);
        drop(fs);
        let fs = mount(&dev); // recovers orphans
        assert!(
            block_bit_set(&fs, victim),
            "block {victim} (the root directory's) was freed by orphan recovery"
        );
        assert!(!block_bit_set(&fs, xb), "the xattr block {xb} was kept");
        assert_eq!(fs.sb.last_orphan, 0, "the orphan chain was not drained");
    }

    /// `s_state` straight off the device, as another handle -- or `e2fsck`
    /// -- would read it.
    fn on_disk_state(dev: &std::sync::Arc<MemDev>) -> u16 {
        let bytes = dev.bytes.lock().unwrap();
        u16::from_le_bytes([bytes[1024 + 0x3A], bytes[1024 + 0x3B]])
    }

    /// A read-write mount that writes marks the volume not clean while it
    /// is mounted, and a drop puts it back (#85). The superblock checksum
    /// stays right through both writes: each remount verifies it.
    #[test]
    fn a_writing_mount_marks_the_volume_not_clean_until_it_is_dropped() {
        use crate::superblock::EXT4_VALID_FS;
        let dev = formatted();
        assert_ne!(
            on_disk_state(&dev) & EXT4_VALID_FS,
            0,
            "fixture: formatted clean"
        );

        let fs = mount(&dev);
        assert_ne!(
            on_disk_state(&dev) & EXT4_VALID_FS,
            0,
            "mounting alone must not touch the superblock"
        );
        fs.apply_create("/a.txt", 0o644).expect("create");
        assert_eq!(
            on_disk_state(&dev) & EXT4_VALID_FS,
            0,
            "a volume being written must not read as cleanly unmounted"
        );
        drop(mount(&dev)); // the superblock checksum still verifies
        drop(fs);
        assert_ne!(
            on_disk_state(&dev) & EXT4_VALID_FS,
            0,
            "the unmount must mark the volume clean again"
        );
        drop(mount(&dev));
    }

    /// A read-write mount that never writes leaves the superblock exactly
    /// as it found it.
    #[test]
    fn a_mount_that_does_not_write_leaves_the_superblock_alone() {
        let dev = formatted();
        let before = dev.bytes.lock().unwrap()[1024..2048].to_vec();
        let fs = mount(&dev);
        fs.read_inode_verified(2).expect("read the root");
        drop(fs);
        assert!(dev.bytes.lock().unwrap()[1024..2048] == before[..]);
    }

    /// A volume that was already not clean -- a crash before this mount --
    /// is not declared clean by this mount's unmount. The kernel restores
    /// the state it mounted with, and so does this.
    #[test]
    fn an_unclean_volume_stays_unclean_after_a_writing_mount() {
        use crate::superblock::EXT4_VALID_FS;
        let dev = formatted();
        {
            let fs = mount(&dev);
            fs.write_superblock_state(fs.sb.state & !EXT4_VALID_FS)
                .expect("mark not clean, as a crash would leave it");
        }
        let fs = mount(&dev);
        assert!(!fs.sb.is_clean(), "fixture: mounted not clean");
        fs.apply_create("/a.txt", 0o644).expect("create");
        drop(fs);
        assert_eq!(on_disk_state(&dev) & EXT4_VALID_FS, 0);
    }

    /// Renaming a path onto itself answers as rename(2) does -- a missing
    /// path is ENOENT, a NUL in the name is refused, an existing path is a
    /// success that changes nothing -- and none of the three marks the
    /// volume not clean, because none of them writes (#303).
    #[test]
    fn a_rename_onto_itself_resolves_the_path_and_writes_nothing() {
        use crate::superblock::EXT4_VALID_FS;
        let dev = formatted();
        {
            let fs = mount(&dev);
            fs.apply_create("/f", 0o644).expect("create");
        }
        assert_ne!(on_disk_state(&dev) & EXT4_VALID_FS, 0, "fixture: clean");

        let fs = mount(&dev);
        let mut wrong = Vec::new();
        let got = fs.apply_rename("/missing", "/missing", false);
        if !matches!(got, Err(Error::NotFound)) {
            wrong.push(format!("a missing path onto itself: {got:?}"));
        }
        let got = fs.apply_rename("/a\0b", "/a\0b", false);
        if !matches!(
            got,
            Err(Error::InvalidArgument("a name cannot contain a NUL byte"))
        ) {
            wrong.push(format!("a NUL name onto itself: {got:?}"));
        }
        if on_disk_state(&dev) & EXT4_VALID_FS == 0 {
            wrong.push("a refused rename marked the volume not clean".into());
        }
        let before = dev.bytes.lock().unwrap().clone();
        let got = fs.apply_rename("/f", "/f", false);
        if got.is_err() {
            wrong.push(format!("an existing path onto itself: {got:?}"));
        }
        if *dev.bytes.lock().unwrap() != before {
            wrong.push("renaming an existing path onto itself wrote".into());
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
        resolve(&fs, "/f").expect("/f is still there");
    }

    /// The case this crate already handled: nothing names the inode any
    /// more, so recovery really does delete it. Kept as the other half of
    /// the pair, so the fix for the truncate case cannot be a blanket
    /// "leave every orphan alone".
    #[test]
    fn an_orphan_with_no_links_is_still_reclaimed() {
        let dev = formatted();
        let ino = {
            let fs = mount(&dev);
            let ino = fs.apply_create("/gone.txt", 0o644).expect("create");
            fs.apply_pwrite("/gone.txt", 0, &[0xAB; 4 * BS as usize])
                .expect("write");
            ino
        };
        {
            let fs = mount(&dev);
            plant_orphan(&fs, ino, 0, None);
        }
        // Recovery runs on this mount; the next one observes the result.
        drop(mount(&dev));

        let fs = mount(&dev);
        assert!(
            fs.orphan_list().expect("orphan_list").is_empty(),
            "recovery must empty the chain"
        );
        let (inode, _) = fs.read_inode_verified(ino).expect("read inode");
        assert_eq!(inode.links_count, 0, "an unlinked orphan stays unlinked");
        assert_ne!(inode.dtime, 0, "and is stamped as deleted");
        assert_eq!(inode.size, 0, "its body is gone");
    }

    /// A device that drops every write after the first `budget`, as power
    /// loss would, and reports success for them.
    struct CrashDev {
        inner: std::sync::Arc<MemDev>,
        budget: usize,
        writes: std::sync::atomic::AtomicUsize,
    }

    impl crate::block_io::BlockDevice for CrashDev {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            self.inner.read_at(offset, buf)
        }
        fn size_bytes(&self) -> u64 {
            self.inner.size_bytes()
        }
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
            let n = self
                .writes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.budget {
                self.inner.write_at(offset, buf)?;
            }
            Ok(())
        }
        fn flush(&self) -> Result<()> {
            Ok(())
        }
        fn is_writable(&self) -> bool {
            true
        }
    }

    /// Unjournaled orphan recovery, cut after every write, never disarms
    /// itself before its work is on disk (#124).
    ///
    /// The unjournaled commit wrote its blocks in ascending block order, so
    /// the superblock -- whose cleared `s_last_orphan` says the work is done
    /// -- went first and the inode table last. A cut in between left an
    /// orphan still allocated, still holding its blocks, and named by
    /// nothing, so no mount would retry it; one cut later its blocks were
    /// free in the bitmap while the inode still mapped them.
    ///
    /// Each cut is judged twice. Straight after the crash, read-only: a
    /// cleared chain head must mean the inode really is deleted. Then after
    /// the next writable mount has had its chance to retry: the orphan must
    /// be gone and fsck must find nothing.
    #[test]
    fn unjournaled_orphan_recovery_survives_a_cut_after_every_write() {
        let dev = formatted();
        let planted = {
            let fs = mount(&dev);
            assert!(
                fs.journal.is_none(),
                "fixture: this path is the unjournaled one"
            );
            let ino = fs.apply_create("/gone.txt", 0o644).expect("create");
            fs.apply_pwrite("/gone.txt", 0, &[0xAB; 4 * BS as usize])
                .expect("write");
            ino
        };
        {
            // Unlinked while open: the name is gone, and only the chain
            // holds the inode and its blocks.
            let fs = mount(&dev);
            let (root, _) = fs.read_inode_verified(2).expect("root");
            let mut buf = BlockBuffer::new(fs.sb.block_size());
            fs.buffer_remove_dir_entry(&mut buf, 2, &root, b"gone.txt")
                .expect("remove the name");
            fs.commit_block_buffer(buf).expect("commit");
            plant_orphan(&fs, planted, 0, None);
        }
        let snapshot = dev.bytes.lock().unwrap().clone();

        let mut total = None;
        for cut in 0.. {
            let image = MemDev::new(VOL);
            *image.bytes.lock().unwrap() = snapshot.clone();
            let crash = std::sync::Arc::new(CrashDev {
                inner: image.clone(),
                budget: cut,
                writes: std::sync::atomic::AtomicUsize::new(0),
            });
            drop(Filesystem::mount(crash.clone()).expect("mount through the cut"));
            let written = crash.writes.load(std::sync::atomic::Ordering::SeqCst);
            total.get_or_insert(written);

            {
                let ro = std::sync::Arc::new(RoDev(image.clone()));
                let fs = Filesystem::mount(ro).expect("read-only mount of the cut image");
                let (inode, _) = fs.read_inode_verified(planted).expect("read orphan");
                if fs.sb.last_orphan == 0 {
                    assert!(
                        inode.dtime != 0 && inode.size == 0,
                        "cut {cut}: the chain head is cleared but the orphan is not deleted \
                         (dtime {}, size {}); no mount will retry it",
                        inode.dtime,
                        inode.size
                    );
                }
            }
            {
                // This mount retries recovery; the next one observes it,
                // since `orphan_list` reads the superblock as mounted.
                drop(mount(&image));
                let fs = mount(&image);
                assert!(
                    fs.orphan_list()
                        .unwrap_or_else(|e| panic!("cut {cut}: orphan_list: {e:?}"))
                        .is_empty(),
                    "cut {cut}: the next mount left the orphan on the chain"
                );
                // Free-count drift is allowed: a retry credits the counts a
                // crashed attempt already credited, and `e2fsck -p` fixes
                // "free blocks count wrong" silently. Anything else is not.
                let report = crate::fsck::audit(&fs, u32::MAX, u32::MAX).expect("audit");
                let serious: Vec<_> = report
                    .anomalies
                    .iter()
                    .filter(|a| {
                        !matches!(
                            a,
                            crate::fsck::Anomaly::BlockGroupFreeCountDrift { .. }
                                | crate::fsck::Anomaly::SuperblockFreeCountDrift { .. }
                        )
                    })
                    .collect();
                assert!(
                    serious.is_empty(),
                    "cut {cut}: after the retry, fsck finds {serious:?}"
                );
                // Reclaimed means free in the inode bitmap. A crash between
                // that bit and the inode table can leave the body unzeroed
                // with no deletion time, which `e2fsck -p` stamps silently;
                // with the bit clear nothing treats it as live.
                assert!(
                    !fs.inode_bit_is_set(planted).expect("inode bitmap"),
                    "cut {cut}: after the retry the orphan is still allocated"
                );
            }
            if cut >= written {
                break;
            }
        }
        assert!(
            total.unwrap_or(0) > 1,
            "the recovery commit must span several writes to cut"
        );
    }

    /// Journaled orphan recovery, cut after every write, leaves an image
    /// whose next mount finishes the job (#125).
    ///
    /// `tests/orphan_recovery_crash_safety.rs`, the file this replaces, cut
    /// a no-op: its fixture had no orphans, so every budget interrupted a
    /// recovery that wrote nothing, and it skipped when the fixture was
    /// missing. Here the chain holds a
    /// real orphan on a volume with a journal, so the recovery is one
    /// journaled transaction, and every cut lands inside it or its
    /// checkpoint. Whatever the cut, the next writable mount -- replaying
    /// the journal and retrying the chain -- must end with the orphan
    /// reclaimed, fsck clean, and the volume still taking writes.
    ///
    /// The volume is this crate's ext3 flavour, the one it formats with a
    /// journal. Without extents, recovery reclaims an orphan's inode but
    /// declines to free indirect-mapped blocks (#124), so the orphan is
    /// planted empty.
    #[test]
    fn journaled_orphan_recovery_survives_a_cut_after_every_write() {
        const JVOL: u64 = 64 * 1024 * 1024;
        let dev = MemDev::new(JVOL);
        crate::mkfs::format_filesystem_with_flavor(
            dev.as_ref(),
            Some("jorphan"),
            None,
            JVOL,
            BS,
            crate::features::FsFlavor::Ext3,
        )
        .expect("format");
        let planted = {
            let fs = mount(&dev);
            assert!(
                fs.journal.is_some(),
                "fixture: this path is the journaled one"
            );
            let ino = fs.apply_create("/gone.txt", 0o644).expect("create");
            let (root, _) = fs.read_inode_verified(2).expect("root");
            let mut buf = BlockBuffer::new(fs.sb.block_size());
            fs.buffer_remove_dir_entry(&mut buf, 2, &root, b"gone.txt")
                .expect("remove the name");
            fs.commit_block_buffer(buf).expect("commit");
            plant_orphan(&fs, ino, 0, None);
            ino
        };
        let snapshot = dev.bytes.lock().unwrap().clone();

        let mut total = None;
        for cut in 0.. {
            let image = MemDev::new(JVOL);
            *image.bytes.lock().unwrap() = snapshot.clone();
            let crash = std::sync::Arc::new(CrashDev {
                inner: image.clone(),
                budget: cut,
                writes: std::sync::atomic::AtomicUsize::new(0),
            });
            drop(Filesystem::mount(crash.clone()).expect("mount through the cut"));
            let written = crash.writes.load(std::sync::atomic::Ordering::SeqCst);
            total.get_or_insert(written);

            // The retry, then a mount that observes it.
            drop(mount(&image));
            let fs = mount(&image);
            assert!(
                fs.orphan_list()
                    .unwrap_or_else(|e| panic!("cut {cut}: orphan_list: {e:?}"))
                    .is_empty(),
                "cut {cut}: the next mount left the orphan on the chain"
            );
            assert!(
                !fs.inode_bit_is_set(planted).expect("inode bitmap"),
                "cut {cut}: after the retry the orphan is still allocated"
            );
            let report = crate::fsck::audit(&fs, u32::MAX, u32::MAX).expect("audit");
            let serious: Vec<_> = report
                .anomalies
                .iter()
                .filter(|a| {
                    !matches!(
                        a,
                        crate::fsck::Anomaly::BlockGroupFreeCountDrift { .. }
                            | crate::fsck::Anomaly::SuperblockFreeCountDrift { .. }
                    )
                })
                .collect();
            assert!(
                serious.is_empty(),
                "cut {cut}: after the retry, fsck finds {serious:?}"
            );
            // And the journal still takes a transaction after recovery.
            fs.apply_create("/after.txt", 0o644)
                .unwrap_or_else(|e| panic!("cut {cut}: a write after recovery failed: {e:?}"));
            if cut >= written {
                break;
            }
        }
        assert!(
            total.unwrap_or(0) > 1,
            "the recovery commit must span several writes to cut"
        );
    }

    /// A chain whose members sit in different block groups, cut after
    /// every write, strands none of them (Greptile on #201).
    ///
    /// The chain's only link from one member to the next is the member's
    /// own `i_dtime`, and reclaiming a member overwrites it. With the
    /// whole chain in one commit, a cut after the head's group was written
    /// and before the second member's left the head free with a deletion
    /// time and the second still allocated; the next mount stopped at the
    /// free head and cleared `s_last_orphan`, and the second member and its
    /// blocks were named by nothing, for good.
    #[test]
    fn unjournaled_recovery_of_a_chain_across_groups_survives_a_cut_after_every_write() {
        // Two groups: 32768 blocks per group at 4 KiB, and a second
        // group this driver's mkfs only makes at a 4 KiB block size.
        const TWO_GROUPS: u64 = 160 * 1024 * 1024;
        let dev = MemDev::new(TWO_GROUPS);
        crate::mkfs::format_filesystem(dev.as_ref(), Some("orphans"), None, TWO_GROUPS, BS)
            .expect("format");
        let members = {
            let fs = mount(&dev);
            assert!(fs.journal.is_none(), "fixture: the unjournaled path");
            let ipg = fs.sb.inodes_per_group;
            let mut by_group = std::collections::BTreeMap::new();
            for d in 0..8 {
                // Remounted each time, so the spread sees the counts the
                // last directory left.
                let fs = mount(&dev);
                fs.apply_mkdir(&format!("/d{d}"), 0o755).expect("mkdir");
                let path = format!("/d{d}/gone");
                let ino = fs.apply_create(&path, 0o644).expect("create");
                fs.apply_pwrite(&path, 0, &[0xCD; 3000]).expect("write");
                by_group.entry((ino - 1) / ipg).or_insert((d, ino));
            }
            let members: Vec<(usize, u32)> = by_group.into_values().take(3).collect();
            assert!(
                members.len() >= 2,
                "fixture: the orphans must sit in different groups, got {by_group_len}",
                by_group_len = members.len()
            );
            members
        };
        {
            // Unlink each while open, and chain them: the head's i_dtime
            // names the second, and so on.
            let fs = mount(&dev);
            for &(d, _) in &members {
                let dir = resolve_ino(&fs, &format!("/d{d}"));
                let (parent, _) = fs.read_inode_verified(dir).expect("dir");
                let mut buf = BlockBuffer::new(fs.sb.block_size());
                fs.buffer_remove_dir_entry(&mut buf, dir, &parent, b"gone")
                    .expect("remove the name");
                fs.commit_block_buffer(buf).expect("commit");
            }
            let mut buf = BlockBuffer::new(fs.sb.block_size());
            for (i, &(_, ino)) in members.iter().enumerate() {
                let next = members.get(i + 1).map_or(0, |&(_, n)| n);
                let (inode, mut raw) = fs.read_inode_verified(ino).expect("read");
                raw[0x1A..0x1C].copy_from_slice(&0u16.to_le_bytes());
                raw[0x14..0x18].copy_from_slice(&next.to_le_bytes());
                fs.finalize_inode_raw(ino, inode.generation, &mut raw)
                    .expect("finalize");
                fs.buffer_write_inode(&mut buf, ino, &raw).expect("write");
            }
            fs.buffer_patch_sb_last_orphan(&mut buf, members[0].1)
                .expect("head");
            fs.commit_block_buffer(buf).expect("commit");
        }
        let snapshot = dev.bytes.lock().unwrap().clone();

        let mut total = None;
        for cut in 0.. {
            let image = MemDev::new(TWO_GROUPS);
            *image.bytes.lock().unwrap() = snapshot.clone();
            let crash = std::sync::Arc::new(CrashDev {
                inner: image.clone(),
                budget: cut,
                writes: std::sync::atomic::AtomicUsize::new(0),
            });
            drop(Filesystem::mount(crash.clone()).expect("mount through the cut"));
            let written = crash.writes.load(std::sync::atomic::Ordering::SeqCst);
            total.get_or_insert(written);

            // The next writable mount retries; the one after observes.
            drop(mount(&image));
            let fs = mount(&image);
            assert!(
                fs.orphan_list()
                    .unwrap_or_else(|e| panic!("cut {cut}: orphan_list: {e:?}"))
                    .is_empty(),
                "cut {cut}: the chain was not cleared"
            );
            for &(_, ino) in &members {
                assert!(
                    !fs.inode_bit_is_set(ino).expect("inode bitmap"),
                    "cut {cut}: orphan {ino} is still allocated and named by nothing"
                );
            }
            let report = crate::fsck::audit(&fs, u32::MAX, u32::MAX).expect("audit");
            let serious: Vec<_> = report
                .anomalies
                .iter()
                .filter(|a| {
                    !matches!(
                        a,
                        crate::fsck::Anomaly::BlockGroupFreeCountDrift { .. }
                            | crate::fsck::Anomaly::SuperblockFreeCountDrift { .. }
                    )
                })
                .collect();
            assert!(
                serious.is_empty(),
                "cut {cut}: after the retry, fsck finds {serious:?}"
            );
            if cut >= written {
                break;
            }
        }
        assert!(total.unwrap_or(0) > 2, "recovery must span several writes");
    }

    /// An orphan whose blocks this cannot reclaim -- mapped the legacy
    /// indirect way, as every ext3 file is -- is left whole and at the head
    /// of the chain (CodeRabbit on #201). Freeing its inode without its
    /// blocks, and moving the head past it, left the blocks allocated and
    /// named by nothing, for good.
    #[test]
    fn an_orphan_whose_blocks_cannot_be_reclaimed_stays_whole_at_the_head() {
        let dev = formatted();
        let ino = {
            let fs = mount(&dev);
            let ino = fs.apply_create("/gone.txt", 0o644).expect("create");
            fs.apply_pwrite("/gone.txt", 0, &[0xAB; 3 * BS as usize])
                .expect("write");
            ino
        };
        {
            let fs = mount(&dev);
            let (root, _) = fs.read_inode_verified(2).expect("root");
            let mut buf = BlockBuffer::new(fs.sb.block_size());
            fs.buffer_remove_dir_entry(&mut buf, 2, &root, b"gone.txt")
                .expect("remove the name");
            // Mapped the legacy way, as far as recovery can tell: the
            // driver cannot write an indirect file to make one.
            let (inode, mut raw) = fs.read_inode_verified(ino).expect("read");
            let flags = inode.flags & !crate::inode::InodeFlags::EXTENTS.bits();
            raw[0x20..0x24].copy_from_slice(&flags.to_le_bytes());
            fs.finalize_inode_raw(ino, inode.generation, &mut raw)
                .expect("finalize");
            fs.buffer_write_inode(&mut buf, ino, &raw)
                .expect("write inode");
            fs.commit_block_buffer(buf).expect("commit");
            plant_orphan(&fs, ino, 0, None);
            let (inode, _) = fs.read_inode_verified(ino).expect("read");
            assert!(
                !inode.has_extents() && inode.blocks > 0,
                "fixture: a non-extent orphan holding blocks"
            );
        }
        // This mount runs recovery; the next one observes what it left.
        drop(mount(&dev));
        let fs = mount(&dev);
        assert_eq!(fs.sb.last_orphan, ino, "the orphan left the chain");
        assert!(
            fs.inode_bit_is_set(ino).expect("bitmap"),
            "the orphan's inode was freed without its blocks"
        );
        let (inode, _) = fs.read_inode_verified(ino).expect("read");
        assert!(inode.blocks > 0, "the orphan's body was zeroed");
    }

    fn resolve_ino(fs: &Filesystem, path: &str) -> u32 {
        let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(inode, _)| inode);
        crate::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, path).expect("resolve")
    }

    // ---------------------------------------------------------------
    // Features that may be read and may not be written
    // ---------------------------------------------------------------

    /// A device that reads and writes like `MemDev` but reports itself
    /// read-only, so a mount takes the read-only path.
    struct RoDev(std::sync::Arc<MemDev>);

    impl crate::block_io::BlockDevice for RoDev {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            self.0.read_at(offset, buf)
        }
        fn size_bytes(&self) -> u64 {
            self.0.size_bytes()
        }
        fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<()> {
            Err(Error::ReadOnly)
        }
        fn flush(&self) -> Result<()> {
            Ok(())
        }
        fn is_writable(&self) -> bool {
            false
        }
    }

    /// Set an INCOMPAT bit on a formatted volume, fixing the superblock
    /// checksum so the result still mounts.
    fn set_incompat_bit(dev: &std::sync::Arc<MemDev>, bit: u32) {
        let mut sb = vec![0u8; 1024];
        dev.read_at(crate::superblock::SUPERBLOCK_OFFSET, &mut sb)
            .expect("read sb");
        let cur = u32::from_le_bytes(sb[0x60..0x64].try_into().unwrap());
        sb[0x60..0x64].copy_from_slice(&(cur | bit).to_le_bytes());
        let csum = crate::checksum::linux_crc32c(!0, &sb[..0x3FC]);
        sb[0x3FC..0x400].copy_from_slice(&csum.to_le_bytes());
        dev.write_at(crate::superblock::SUPERBLOCK_OFFSET, &sb)
            .expect("write sb");
    }

    /// Turn the directory `dir` (child of `parent`) into an inline-data
    /// directory the way the kernel lays one out: `i_block` holds the
    /// parent's inode number, then entries; `i_size` is 60. `entry`, if
    /// given, is `(inode, name, file_type)` placed as its one entry.
    fn make_inline_dir(fs: &Filesystem, dir: u32, parent: u32, entry: Option<(u32, &[u8], u8)>) {
        let (inode, mut raw) = fs.read_inode_verified(dir).unwrap();
        let mut flags = u32::from_le_bytes(raw[0x20..0x24].try_into().unwrap());
        flags &= !crate::inode::InodeFlags::EXTENTS.bits();
        flags |= crate::inode::InodeFlags::INLINE_DATA.bits();
        raw[0x20..0x24].copy_from_slice(&flags.to_le_bytes());
        raw[0x28..0x64].fill(0);
        raw[0x28..0x2C].copy_from_slice(&parent.to_le_bytes());
        let (ino, name, ft) = entry.unwrap_or((0, b"", 0));
        raw[0x2C..0x30].copy_from_slice(&ino.to_le_bytes());
        raw[0x30..0x32].copy_from_slice(&56u16.to_le_bytes());
        raw[0x32] = name.len() as u8;
        raw[0x33] = ft;
        raw[0x34..0x34 + name.len()].copy_from_slice(name);
        raw[0x04..0x08].copy_from_slice(&60u32.to_le_bytes());
        fs.finalize_inode_raw(dir, inode.generation, &mut raw)
            .unwrap();
        fs.write_inode_raw(dir, &raw).unwrap();
    }

    /// The image outside the primary superblock, which a mount's first write
    /// marks not clean and its drop marks clean again.
    fn outside_superblock(dev: &std::sync::Arc<MemDev>) -> Vec<u8> {
        let mut bytes = dev.bytes.lock().unwrap().clone();
        bytes[1024..2048].fill(0);
        bytes
    }

    /// #382: renaming an inline-data directory must not treat its i_block
    /// (parent inode number, then entries) as a block map.
    #[test]
    fn renaming_an_inline_directory_does_not_write_through_its_parent_number() {
        let dev = formatted();
        set_incompat_bit(&dev, crate::features::Incompat::INLINE_DATA.bits());
        let fs = mount(&dev);
        fs.apply_mkdir("/a", 0o755).unwrap();
        fs.apply_mkdir("/b", 0o755).unwrap();
        fs.apply_mkdir("/a/sub", 0o755).unwrap();
        let a = resolve(&fs, "/a").unwrap();
        let sub = resolve(&fs, "/a/sub").unwrap();
        make_inline_dir(&fs, sub, a, None);
        drop(fs);
        let fs = mount(&dev);
        let before = fs.read_block(a as u64).unwrap();
        let r = fs.apply_rename("/a/sub", "/b/sub", false);
        drop(fs);
        let fs = mount(&dev);
        let after = fs.read_block(a as u64).unwrap();
        assert!(
            before == after,
            "block {a} (the parent's inode number) was written by a rename: {r:?}"
        );
        assert!(
            matches!(r, Err(Error::Unsupported(_))),
            "rename of an inline directory: {r:?}"
        );
    }

    /// #382: every directory mutation whose parent or target is an
    /// inline-data directory is refused, and writes nothing.
    #[test]
    fn every_mutation_of_an_inline_directory_is_refused_and_writes_nothing() {
        let dev = formatted();
        set_incompat_bit(&dev, crate::features::Incompat::INLINE_DATA.bits());
        let fs = mount(&dev);
        fs.apply_mkdir("/a", 0o755).unwrap();
        fs.apply_mkdir("/b", 0o755).unwrap();
        fs.apply_mkdir("/a/sub", 0o755).unwrap();
        fs.apply_mkdir("/a/empty", 0o755).unwrap();
        fs.apply_create("/f2", 0o644).unwrap();
        fs.apply_create("/b/y", 0o644).unwrap();
        let a = resolve(&fs, "/a").unwrap();
        let sub = resolve(&fs, "/a/sub").unwrap();
        let empty = resolve(&fs, "/a/empty").unwrap();
        let f2 = resolve(&fs, "/f2").unwrap();
        // /a/sub holds one entry, "f", a second link to /f2.
        make_inline_dir(&fs, sub, a, Some((f2, b"f", 1)));
        make_inline_dir(&fs, empty, a, None);
        let (i, raw) = fs.read_inode_verified(f2).unwrap();
        let mut raw = raw.clone();
        raw[0x1A..0x1C].copy_from_slice(&2u16.to_le_bytes());
        fs.finalize_inode_raw(f2, i.generation, &mut raw).unwrap();
        fs.write_inode_raw(f2, &raw).unwrap();
        drop(fs);

        type Op = fn(&Filesystem) -> Result<()>;
        let ops: [(&str, Op); 9] = [
            ("create inside", |fs| {
                fs.apply_create("/a/sub/g", 0o644).map(drop)
            }),
            ("mkdir inside", |fs| {
                fs.apply_mkdir("/a/sub/h", 0o755).map(drop)
            }),
            ("symlink inside", |fs| {
                fs.apply_symlink("t", "/a/sub/s").map(drop)
            }),
            ("link into", |fs| fs.apply_link("/f2", "/a/sub/l")),
            ("unlink inside", |fs| fs.apply_unlink("/a/sub/f")),
            ("rename out of", |fs| {
                fs.apply_rename("/a/sub/f", "/b/f", false)
            }),
            ("rename into", |fs| {
                fs.apply_rename("/b/y", "/a/sub/y", false)
            }),
            ("rename over", |fs| fs.apply_rename("/b", "/a/empty", true)),
            ("rmdir", |fs| fs.apply_rmdir("/a/empty")),
        ];
        for (name, op) in ops {
            let before = outside_superblock(&dev);
            let fs = mount(&dev);
            let r = op(&fs);
            drop(fs);
            assert!(
                matches!(r, Err(Error::Unsupported(_))),
                "{name} an inline directory: {r:?}"
            );
            assert!(
                outside_superblock(&dev) == before,
                "{name} an inline directory wrote to the image"
            );
        }
    }

    /// `fs_ext4_listxattr` into a short buffer writes whole names only,
    /// as `include/fs_ext4.h` now says; it said "as much as fits".
    #[test]
    fn listxattr_into_a_short_buffer_writes_whole_names_only() {
        let dev = formatted();
        {
            let fs = mount(&dev);
            fs.apply_create("/x.txt", 0o644).expect("create");
            fs.apply_setxattr("/x.txt", "user.first", b"1")
                .expect("set");
            fs.apply_setxattr("/x.txt", "user.second", b"2")
                .expect("set");
        }
        let image = fs_ext4_test_support::temp_path!("fs_ext4_lx_whole_{}.img", std::process::id());
        std::fs::write(&image, &*dev.bytes.lock().unwrap()).expect("write image");
        let c_image = std::ffi::CString::new(image.clone()).unwrap();
        let c_path = std::ffi::CString::new("/x.txt").unwrap();
        unsafe {
            let handle = crate::capi::fs_ext4_mount(c_image.as_ptr());
            assert!(!handle.is_null(), "C mount");
            let needed =
                crate::capi::fs_ext4_listxattr(handle, c_path.as_ptr(), std::ptr::null_mut(), 0);
            let mut full = vec![0u8; needed as usize];
            crate::capi::fs_ext4_listxattr(
                handle,
                c_path.as_ptr(),
                full.as_mut_ptr().cast(),
                full.len(),
            );
            let first_len = full.iter().position(|&b| b == 0).unwrap() + 1;
            assert!((first_len as i64) < needed, "fixture: two names");

            // Room for the first name and three bytes of the second.
            let mut short = vec![0xEEu8; first_len + 3];
            let got = crate::capi::fs_ext4_listxattr(
                handle,
                c_path.as_ptr(),
                short.as_mut_ptr().cast(),
                short.len(),
            );
            assert_eq!(got, needed, "the required size is still returned");
            assert_eq!(
                &short[..first_len],
                &full[..first_len],
                "the first name, whole"
            );
            assert_eq!(
                &short[first_len..],
                &[0xEE; 3],
                "no part of the second name"
            );
            crate::capi::fs_ext4_umount(handle);
        }
        let _ = std::fs::remove_file(&image);
    }

    /// fsck repair is a write, and refuses what every other write refuses
    /// (#119).
    ///
    /// The repair gate checked only that the device was writable. The
    /// control is the point: the same volume, in the same process, refuses
    /// a create, and an audit without repair still runs.
    #[test]
    fn fsck_repair_refuses_a_volume_whose_features_refuse_writes() {
        let dev = formatted();
        let mut sb = vec![0u8; 1024];
        dev.read_at(crate::superblock::SUPERBLOCK_OFFSET, &mut sb)
            .unwrap();
        let cur = u32::from_le_bytes(sb[0x64..0x68].try_into().unwrap());
        let quota = crate::features::RoCompat::QUOTA.bits();
        sb[0x64..0x68].copy_from_slice(&(cur | quota).to_le_bytes());
        let csum = crate::checksum::linux_crc32c(!0, &sb[..0x3FC]);
        sb[0x3FC..0x400].copy_from_slice(&csum.to_le_bytes());
        dev.write_at(crate::superblock::SUPERBLOCK_OFFSET, &sb)
            .unwrap();

        let fs = mount(&dev);
        assert!(
            matches!(
                fs.apply_create("/x", 0o644),
                Err(Error::UnsupportedRoCompat(_))
            ),
            "control: an ordinary write is refused on this volume"
        );
        fs.audit_repair(u32::MAX, u32::MAX, false)
            .expect("an audit without repair still runs");
        match fs.audit_repair(u32::MAX, u32::MAX, true) {
            Err(Error::UnsupportedRoCompat(bits)) => assert_eq!(bits & quota, quota),
            other => panic!("repair must be refused, got {:?}", other.map(|_| ())),
        }
    }

    /// And an ordinary volume still repairs.
    #[test]
    fn fsck_repair_still_runs_on_an_ordinary_volume() {
        let dev = formatted();
        let fs = mount(&dev);
        fs.audit_repair(u32::MAX, u32::MAX, true)
            .expect("repair on an ordinary volume");
    }

    /// A CASEFOLD volume must not be mounted writable.
    ///
    /// The kernel files a directory entry into the htree leaf that the
    /// SipHash of the case-folded name selects; this driver hashes the
    /// raw bytes. Reads survive on the linear-scan fallback. A write does
    /// not: the entry lands in the wrong leaf, stays findable here and
    /// stops being findable on Linux.
    #[test]
    fn a_casefold_volume_is_not_mounted_writable() {
        let dev = formatted();
        set_incompat_bit(&dev, crate::features::Incompat::CASEFOLD.bits());

        let err = match Filesystem::mount(dev.clone()) {
            Ok(_) => panic!("a writable mount of a CASEFOLD volume must be refused"),
            Err(e) => e,
        };
        match err {
            Error::UnsupportedIncompat(bits) => assert_eq!(
                bits,
                crate::features::Incompat::CASEFOLD.bits(),
                "the refusal must name the bit responsible"
            ),
            other => panic!("expected UnsupportedIncompat, got {other:?}"),
        }
    }

    /// And it must still be READABLE, which is the whole reason the
    /// refusal is scoped to writable mounts rather than to the volume.
    #[test]
    fn a_casefold_volume_still_mounts_read_only() {
        let dev = formatted();
        {
            let fs = mount(&dev);
            fs.apply_create("/before.txt", 0o644).expect("create");
        }
        set_incompat_bit(&dev, crate::features::Incompat::CASEFOLD.bits());

        let ro = std::sync::Arc::new(RoDev(dev.clone()));
        let fs = Filesystem::mount(ro).expect("a read-only mount must still work");
        assert_eq!(
            resolve(&fs, "/before.txt").expect("the directory is still readable"),
            resolve(&fs, "/before.txt").expect("stable"),
        );
    }

    /// A device that turns writable after the mount, as the FSKit write FD
    /// does for a lazy mount.
    struct LaterWritable {
        inner: std::sync::Arc<MemDev>,
        writable: std::sync::atomic::AtomicBool,
    }

    impl crate::block_io::BlockDevice for LaterWritable {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            self.inner.read_at(offset, buf)
        }
        fn size_bytes(&self) -> u64 {
            self.inner.size_bytes()
        }
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
            if !self.is_writable() {
                return Err(Error::ReadOnly);
            }
            self.inner.write_at(offset, buf)
        }
        fn flush(&self) -> Result<()> {
            Ok(())
        }
        fn is_writable(&self) -> bool {
            self.writable.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// A lazy mount that was read-only when it mounted and writable later
    /// still refuses to write a write-breaking INCOMPAT volume (#117).
    ///
    /// The refusal ran once at mount, and passed because the device was
    /// not yet writable; every later write checked only RO_COMPAT.
    #[test]
    fn a_volume_that_becomes_writable_after_a_lazy_mount_still_refuses_write_breaking_features() {
        for bit in [
            crate::features::Incompat::CASEFOLD.bits(),
            crate::features::Incompat::MMP.bits(),
        ] {
            let dev = formatted();
            set_incompat_bit(&dev, bit);
            let later = std::sync::Arc::new(LaterWritable {
                inner: dev.clone(),
                writable: std::sync::atomic::AtomicBool::new(false),
            });
            let fs = Filesystem::mount_lazy(later.clone()).expect("a read-only lazy mount");
            later
                .writable
                .store(true, std::sync::atomic::Ordering::SeqCst);
            match fs.apply_create("/late.txt", 0o644) {
                Err(Error::UnsupportedIncompat(b)) => assert_eq!(b, bit),
                other => panic!("{bit:#x}: a write went through: {:?}", other.map(|_| ())),
            }
        }
    }

    /// The control: the same late-writable device over an ordinary volume
    /// does write, so the refusal above is the feature and not the device.
    #[test]
    fn an_ordinary_volume_that_becomes_writable_after_a_lazy_mount_writes() {
        let dev = formatted();
        let later = std::sync::Arc::new(LaterWritable {
            inner: dev,
            writable: std::sync::atomic::AtomicBool::new(false),
        });
        let fs = Filesystem::mount_lazy(later.clone()).expect("a read-only lazy mount");
        later
            .writable
            .store(true, std::sync::atomic::Ordering::SeqCst);
        fs.apply_create("/late.txt", 0o644)
            .expect("an ordinary volume must accept the write");
    }

    /// The MMP case this generalised, so folding the two into one set
    /// cannot have dropped the original.
    #[test]
    fn an_mmp_volume_is_still_not_mounted_writable() {
        let dev = formatted();
        set_incompat_bit(&dev, crate::features::Incompat::MMP.bits());
        let err = match Filesystem::mount(dev.clone()) {
            Ok(_) => panic!("a writable mount of an MMP volume must be refused"),
            Err(e) => e,
        };
        assert!(matches!(
            err,
            Error::UnsupportedIncompat(b) if b == crate::features::Incompat::MMP.bits()
        ));
    }

    /// The control: an ordinary volume still mounts writable and still
    /// accepts a create, so none of the above can be satisfied by
    /// refusing every writable mount.
    #[test]
    fn an_ordinary_volume_still_mounts_writable() {
        let dev = formatted();
        assert_eq!(
            Superblock::read(dev.as_ref()).unwrap().last_orphan,
            0,
            "new formatter must not overlap hash seed with orphan head"
        );
        let fs = Filesystem::mount(dev.clone()).expect("mount");
        fs.apply_create("/after.txt", 0o644).expect("create");
    }

    // ---------------------------------------------------------------
    // The refusal has to come BEFORE journal replay
    // ---------------------------------------------------------------
    //
    // Replay is not a read. It takes the blocks a previous writer
    // committed to the log and writes them into the filesystem proper.
    // Doing that to a volume carrying a feature this driver does not
    // maintain is the exact damage `write_breaking_incompat` exists to
    // prevent, so the refusal has to happen first.
    //
    // On `main` it does, by five lines. Nothing holds it there: move the
    // refusal below `replay_if_dirty` and every other test still passes,
    // because the only volumes that carry one of these bits today are
    // `formatted()` ext4 images and this crate's mkfs writes no journal
    // for them. The resulting driver would decline the mount *after*
    // having already written to the disk.
    //
    // These two tests fix the order by giving it a witness: a volume
    // that has both an unsupported bit and a genuinely replayable
    // journal, and a destination block whose contents say whether the
    // replay ran.

    /// The block a planted transaction writes to. Chosen well past the
    /// metadata and the 1024-block journal mkfs lays down for ext3, and
    /// well inside a 32 MiB device, so replay's own bounds checks have
    /// no reason to refuse it.
    const REPLAY_TARGET_BLOCK: u64 = 4000;

    /// Format an ext3 volume — the one flavour whose mkfs writes a real
    /// journal — and leave a single committed transaction in it that
    /// replay has not yet applied.
    ///
    /// Returns the device and the payload the transaction will write to
    /// `REPLAY_TARGET_BLOCK`, which is what the caller checks for.
    fn ext3_with_a_dirty_journal() -> (std::sync::Arc<MemDev>, Vec<u8>) {
        let dev = MemDev::new(VOL);
        crate::mkfs::format_filesystem_with_flavor(
            dev.as_ref(),
            Some("replay"),
            None,
            VOL,
            BS,
            crate::features::FsFlavor::Ext3,
        )
        .expect("format ext3");

        let payload = {
            let fs = Filesystem::mount(dev.clone()).expect("mount to plant the journal");
            let block_size = fs.sb.block_size() as u64;

            let raw = fs
                .read_inode_raw(fs.sb.journal_inode)
                .expect("read the journal inode");
            let jinode = crate::inode::Inode::parse(&raw).expect("parse the journal inode");
            let jsb = crate::jbd2::read_superblock(&fs)
                .expect("read the journal superblock")
                .expect("ext3 has a journal");

            // One transaction, one write tag: descriptor, data, commit.
            let mut tx = crate::transaction::Transaction::begin(
                jsb.sequence,
                block_size as u32,
                jsb.uses_64bit(),
                jsb.feature_incompat & crate::jbd2::JbdIncompat::CSUM_V3.bits() != 0,
            );
            let payload: Vec<u8> = (0..block_size as usize)
                .map(|i| 0xA5u8.wrapping_add((i & 0xFF) as u8))
                .collect();
            tx.add_write(REPLAY_TARGET_BLOCK, payload.clone())
                .expect("add_write");
            let blocks = tx.commit().expect("commit");
            assert_eq!(blocks.len(), 3, "descriptor + data + commit");

            // Journal logical block 0 is the journal superblock, so the
            // log itself starts at 1.
            for (i, blk) in blocks.iter().enumerate() {
                let phys = crate::jbd2::journal_block_to_physical(&fs, &jinode, (i as u64) + 1)
                    .expect("map the journal block")
                    .expect("the journal is contiguous, so it is mapped");
                fs.dev
                    .write_at(phys * block_size, blk)
                    .expect("write the journal slot");
            }

            // s_start is at offset 0x1C and JBD2 is big-endian. Setting
            // it to 1 is what makes the journal dirty.
            let jsb_phys = crate::jbd2::journal_block_to_physical(&fs, &jinode, 0)
                .expect("map the journal superblock")
                .expect("mapped");
            let mut jsb_bytes = vec![0u8; block_size as usize];
            fs.dev
                .read_at(jsb_phys * block_size, &mut jsb_bytes)
                .expect("read the journal superblock");
            assert_eq!(
                u32::from_be_bytes(jsb_bytes[0..4].try_into().unwrap()),
                crate::jbd2::JBD2_MAGIC_NUMBER,
                "the block the journal inode maps is not a JBD2 superblock"
            );
            jsb_bytes[0x1C..0x20].copy_from_slice(&1u32.to_be_bytes());
            fs.dev
                .write_at(jsb_phys * block_size, &jsb_bytes)
                .expect("write the journal superblock");
            fs.dev.flush().expect("flush");
            payload
        };

        assert!(
            !destination_holds_the_payload(&dev, &payload),
            "the destination already holds the payload, so replaying could not be observed"
        );
        (dev, payload)
    }

    /// Whether the transaction's payload has reached its destination —
    /// which is to say, whether replay ran.
    ///
    /// Returns a bool rather than the block so a failure prints one line
    /// instead of two 4096-byte vectors.
    fn destination_holds_the_payload(dev: &std::sync::Arc<MemDev>, payload: &[u8]) -> bool {
        let mut buf = vec![0u8; BS as usize];
        crate::block_io::BlockDevice::read_at(
            dev.as_ref(),
            REPLAY_TARGET_BLOCK * BS as u64,
            &mut buf,
        )
        .expect("read the destination block");
        buf == payload
    }

    /// The control. Without an unsupported bit, this volume's journal
    /// really does replay at mount, so the assertion below it — that
    /// the destination is untouched — is a statement about the refusal
    /// and not about a journal that was never going to replay anyway.
    #[test]
    fn a_dirty_journal_is_replayed_at_mount() {
        let (dev, payload) = ext3_with_a_dirty_journal();
        let fs = Filesystem::mount(dev.clone()).expect("mount");
        drop(fs);
        assert!(
            destination_holds_the_payload(&dev, &payload),
            "mount did not replay a dirty journal"
        );
    }

    /// And with one, the mount is refused and the journal is left alone.
    ///
    /// The refusal on its own proves nothing: a driver that replayed
    /// first and refused afterwards would still return this error. What
    /// separates the two orders is whether the log was consumed, which
    /// is what the second assertion reads.
    #[test]
    fn an_unsupported_bit_is_refused_before_the_journal_is_replayed() {
        let (dev, payload) = ext3_with_a_dirty_journal();
        set_incompat_bit(&dev, crate::features::Incompat::CASEFOLD.bits());

        match Filesystem::mount(dev.clone()) {
            Ok(_) => panic!("a writable mount of a CASEFOLD volume must be refused"),
            Err(Error::UnsupportedIncompat(bits)) => assert_eq!(
                bits,
                crate::features::Incompat::CASEFOLD.bits(),
                "the refusal must name the bit responsible"
            ),
            Err(other) => panic!("expected UnsupportedIncompat, got {other:?}"),
        }

        assert!(
            !destination_holds_the_payload(&dev, &payload),
            "the journal was replayed into a volume the driver had already declined to mount"
        );
    }

    /// A lazy mount that turns writable does not replay a dirty journal
    /// into a write-breaking INCOMPAT volume either (#117): replay is a
    /// write, and it did not go through `refuse_write`. The control
    /// replays the same journal once the bit is gone.
    #[test]
    fn a_lazy_mount_that_becomes_writable_does_not_replay_into_a_write_breaking_volume() {
        for bit in [0, crate::features::Incompat::MMP.bits()] {
            let (dev, payload) = ext3_with_a_dirty_journal();
            if bit != 0 {
                set_incompat_bit(&dev, bit);
            }
            let later = std::sync::Arc::new(LaterWritable {
                inner: dev.clone(),
                writable: std::sync::atomic::AtomicBool::new(false),
            });
            let mut fs = Filesystem::mount_lazy(later.clone()).expect("a read-only lazy mount");
            later
                .writable
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let result = fs.replay_journal_if_dirty();
            if bit == 0 {
                assert_eq!(result.expect("the control replays"), 1);
                assert!(destination_holds_the_payload(&dev, &payload));
            } else {
                match result {
                    Err(Error::UnsupportedIncompat(b)) => assert_eq!(b, bit),
                    other => panic!("replay was not refused: {other:?}"),
                }
                assert!(
                    !destination_holds_the_payload(&dev, &payload),
                    "the journal was replayed into an MMP volume"
                );
            }
        }
    }

    /// A writable lazy mount must not commit over a journal it has not
    /// replayed (#375). The first commit writes its descriptor at journal
    /// block 1, over the unreplayed log, and then marks the journal clean:
    /// every transaction the last writer committed but did not checkpoint
    /// is gone. The next mount therefore finds nothing to replay, and the
    /// payload the journal carried never reaches its destination.
    #[test]
    fn a_lazy_mount_refuses_writes_until_its_journal_is_replayed() {
        let (dev, payload) = ext3_with_a_dirty_journal();
        let fs = Filesystem::mount_lazy(dev.clone()).expect("lazy mount");
        let r = fs.apply_mkdir("/x", 0o755);
        drop(fs);
        let _ = Filesystem::mount(dev.clone());
        assert!(
            destination_holds_the_payload(&dev, &payload),
            "the unreplayed transaction was lost (write result: {r:?})"
        );
    }

    /// The refusal names its reason, and it lifts once the journal is
    /// replayed: the same handle then writes, and the replayed transaction
    /// is still on disk afterwards (#375).
    #[test]
    fn a_lazy_mount_writes_once_its_journal_is_replayed() {
        let (dev, payload) = ext3_with_a_dirty_journal();
        let mut fs = Filesystem::mount_lazy(dev.clone()).expect("lazy mount");
        match fs.apply_mkdir("/x", 0o755) {
            Err(Error::JournalNotReplayed) => {}
            other => panic!("a write over an unreplayed journal: {other:?}"),
        }
        assert_eq!(fs.replay_journal_if_dirty().expect("replay"), 1);
        fs.apply_mkdir("/x", 0o755)
            .expect("the write is accepted once the journal is replayed");
        drop(fs);
        assert!(destination_holds_the_payload(&dev, &payload));
    }

    /// A lazy mount of a clean journal has nothing to replay, so it writes
    /// straight away: the refusal is about the journal, not the lazy mount.
    #[test]
    fn a_lazy_mount_of_a_clean_journal_writes_without_a_replay() {
        let (dev, _payload) = ext3_with_a_dirty_journal();
        drop(Filesystem::mount(dev.clone()).expect("eager mount replays"));
        let fs = Filesystem::mount_lazy(dev.clone()).expect("lazy mount");
        fs.apply_mkdir("/x", 0o755)
            .expect("a clean journal needs no replay before a write");
    }

    /// The read-only-then-writable shape (#375): the mount replayed the
    /// journal into the cache only, so the log on disk is still dirty when
    /// the device turns writable. A write then must wait for the replay
    /// too, or it lands on a volume whose next mount replays the older
    /// log over it.
    #[test]
    fn a_lazy_mount_that_becomes_writable_refuses_writes_until_its_journal_is_replayed() {
        let (dev, payload) = ext3_with_a_dirty_journal();
        let later = std::sync::Arc::new(LaterWritable {
            inner: dev.clone(),
            writable: std::sync::atomic::AtomicBool::new(false),
        });
        let mut fs = Filesystem::mount_lazy(later.clone()).expect("a read-only lazy mount");
        later
            .writable
            .store(true, std::sync::atomic::Ordering::SeqCst);
        match fs.apply_mkdir("/x", 0o755) {
            Err(Error::JournalNotReplayed) => {}
            other => panic!("a write over an unreplayed journal: {other:?}"),
        }
        assert_eq!(fs.replay_journal_if_dirty().expect("replay"), 1);
        assert!(destination_holds_the_payload(&dev, &payload));
        fs.apply_mkdir("/x", 0o755)
            .expect("the write is accepted once the journal is replayed");
    }

    /// After a lazy replay the next commit must carry a sequence past the
    /// replayed transaction, as an eager mount's does (#376). The writer
    /// opened over the dirty journal kept its pre-replay sequence and wrote
    /// it back on the next commit, below the replayed tail: a crash then lets
    /// a later replay walk on into the older transaction behind ours.
    #[test]
    fn a_lazy_replay_leaves_the_writer_past_the_replayed_sequence() {
        let seq_after = |lazy: bool| {
            let (dev, _payload) = ext3_with_a_dirty_journal();
            let fs = if lazy {
                let mut fs = Filesystem::mount_lazy(dev.clone()).expect("lazy");
                assert_eq!(fs.replay_journal_if_dirty().expect("replay"), 1);
                fs
            } else {
                Filesystem::mount(dev.clone()).expect("eager")
            };
            fs.apply_chmod("/", 0o700).expect("chmod");
            crate::jbd2::read_superblock(&fs).unwrap().unwrap().sequence
        };
        assert_eq!(
            seq_after(true),
            seq_after(false),
            "lazy and eager must agree on the log's sequence"
        );
    }

    /// A lazy mount that was read-only when it mounted has no journal writer,
    /// and nothing opened one once the device turned writable, so every write
    /// after the replay call went to the device unjournaled (#376). The
    /// replay call is where the writable handle begins, so it opens one.
    #[test]
    fn a_lazy_mount_that_becomes_writable_journals_after_the_replay_call() {
        let (dev, _payload) = ext3_with_a_dirty_journal();
        let later = std::sync::Arc::new(LaterWritable {
            inner: dev.clone(),
            writable: std::sync::atomic::AtomicBool::new(false),
        });
        let mut fs = Filesystem::mount_lazy(later.clone()).expect("a read-only lazy mount");
        assert!(fs.journal.is_none(), "a read-only mount opens no writer");
        later
            .writable
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(fs.replay_journal_if_dirty().expect("replay"), 1);
        assert!(
            fs.journal.is_some(),
            "the handle is writable now, and its writes must be journaled"
        );
        fs.apply_chmod("/", 0o700).expect("chmod");
        let jsb = crate::jbd2::read_superblock(&fs).unwrap().unwrap();
        let eager = {
            let (dev, _payload) = ext3_with_a_dirty_journal();
            let fs = Filesystem::mount(dev.clone()).expect("eager");
            fs.apply_chmod("/", 0o700).expect("chmod");
            crate::jbd2::read_superblock(&fs).unwrap().unwrap().sequence
        };
        assert_eq!(
            jsb.sequence, eager,
            "the chmod went through the journal, past the replayed sequence"
        );
    }

    // ---------------------------------------------------------------
    // EA_INODE: an attribute whose value lives in another inode
    // ---------------------------------------------------------------

    /// Give `ino` an in-inode xattr named `name` whose `e_value_inum`
    /// points at `value_inum`, while the bytes at `e_value_offs` are
    /// `decoy` — which is the shape that makes the defect quiet. The
    /// decoy is a real, in-range value, so a parser that ignores
    /// `e_value_inum` returns plausible bytes rather than failing.
    fn plant_ea_inode_xattr(fs: &Filesystem, ino: u32, name: &str, value_inum: u32, decoy: &[u8]) {
        let (inode, mut raw) = fs.read_inode_verified(ino).expect("read inode");
        let inode_size = fs.sb.inode_size as usize;
        let extra_isize = u16::from_le_bytes(raw[128..130].try_into().unwrap()) as usize;
        let start = 128 + extra_isize;
        {
            let region = &mut raw[start..inode_size];
            crate::xattr::plan_set_in_inode_region(region, name, decoy).expect("set xattr");
            // The entry table begins after the 4-byte magic; this is the
            // only entry, so it is the first one. Point it at the EA
            // inode and leave `e_value_offs` and `e_value_size` alone, so
            // the decoy stays exactly where a careless read would find it.
            let e = 4;
            region[e + 4..e + 8].copy_from_slice(&value_inum.to_le_bytes());
        }
        let mut buf = BlockBuffer::new(fs.sb.block_size());
        fs.finalize_inode_raw(ino, inode.generation, &mut raw)
            .expect("finalize");
        fs.buffer_write_inode(&mut buf, ino, &raw).expect("write");
        fs.commit_block_buffer(buf).expect("commit");
    }

    /// Turn an ordinary file into an EA inode: set the flag its own
    /// reader checks for, and leave its body as the attribute's value.
    fn mark_as_ea_inode(fs: &Filesystem, ino: u32) {
        let (inode, mut raw) = fs.read_inode_verified(ino).expect("read inode");
        let flags = u32::from_le_bytes(raw[0x20..0x24].try_into().unwrap());
        let flags = flags | crate::inode::InodeFlags::EA_INODE.bits();
        raw[0x20..0x24].copy_from_slice(&flags.to_le_bytes());
        let mut buf = BlockBuffer::new(fs.sb.block_size());
        fs.finalize_inode_raw(ino, inode.generation, &mut raw)
            .expect("finalize");
        fs.buffer_write_inode(&mut buf, ino, &raw).expect("write");
        fs.commit_block_buffer(buf).expect("commit");
    }

    const DECOY: [u8; 64] = [0xEE; 64];

    fn ea_inode_volume() -> (std::sync::Arc<MemDev>, Vec<u8>) {
        let dev = formatted();
        let real: Vec<u8> = (0..64u8)
            .map(|i| i.wrapping_mul(7).wrapping_add(3))
            .collect();
        {
            let fs = mount(&dev);
            fs.apply_create("/subject.txt", 0o644).expect("create");
            let ea = fs.apply_create("/value.bin", 0o644).expect("create ea");
            fs.apply_pwrite("/value.bin", 0, &real)
                .expect("write value");
            mark_as_ea_inode(&fs, ea);
            let subject = resolve(&fs, "/subject.txt").expect("resolve");
            plant_ea_inode_xattr(&fs, subject, "user.big", ea, &DECOY);
        }
        (dev, real)
    }

    /// THE BYTES THE ATTRIBUTE ACTUALLY HOLDS. Both parsers read
    /// `e_value_inum` and dropped it, then sliced the value out of the
    /// region at `e_value_offs` — which for an EA-inode entry points at
    /// nothing in particular, and here points at a decoy.
    #[test]
    fn an_ea_inode_backed_value_is_read_from_the_inode_it_names() {
        let (dev, real) = ea_inode_volume();
        let fs = mount(&dev);
        let ino = resolve(&fs, "/subject.txt").expect("resolve");
        let (inode, raw) = fs.read_inode_verified(ino).expect("read inode");

        let got = crate::xattr::get_resolved(&fs, &inode, &raw, "user.big")
            .expect("get")
            .expect("the attribute is present");

        assert_ne!(
            got, DECOY,
            "the value was read from e_value_offs instead of from the EA inode"
        );
        assert_eq!(got, real, "the value must be the EA inode's file body");
    }

    /// THE OTHER DOOR. `get_resolved` is the single-attribute entry
    /// point and `read_all_resolved` is the list-all one, and they are
    /// separate functions with separate resolution. `capi.rs` reaches the
    /// first from `fs_ext4_getxattr`; `fs_ext4_listxattr` returns names
    /// only and uses `list_names`, which resolves nothing (#122).
    ///
    /// A Rust consumer enumerating attributes WITH their values rather
    /// than asking for one by name goes through the list-all path, so
    /// leaving it unresolved hands back an empty value with a success
    /// return: the same failure this issue is about, through a different
    /// door.
    #[test]
    fn the_list_all_path_resolves_an_ea_inode_value_too() {
        let (dev, real) = ea_inode_volume();
        let fs = mount(&dev);
        let ino = resolve(&fs, "/subject.txt").expect("resolve");
        let (inode, raw) = fs.read_inode_verified(ino).expect("read inode");

        let entries =
            crate::xattr::read_all_resolved(&fs, &inode, &raw).expect("read_all_resolved");
        let e = entries
            .iter()
            .find(|e| e.name == "user.big")
            .expect("the attribute is present");

        assert!(
            !e.value.is_empty(),
            "the list-all path returned an empty value with a success return"
        );
        assert_ne!(
            e.value, DECOY,
            "the value was read from e_value_offs instead of from the EA inode"
        );
        assert_eq!(e.value, real, "the value must be the EA inode's file body");
    }

    /// A names-only listing does not read values, so a value that cannot be
    /// read does not fail it (#122).
    ///
    /// `fs_ext4_listxattr` resolved every EA-inode value and discarded it,
    /// and one unreadable value turned the whole listing into -1. Here the
    /// attribute points at an inode WITHOUT the EA_INODE flag, which
    /// `read_value_inode` refuses: resolving values fails (the control),
    /// and listing names still names it.
    #[test]
    fn listing_names_does_not_read_an_unreadable_ea_inode_value() {
        let dev = formatted();
        {
            let fs = mount(&dev);
            fs.apply_create("/subject.txt", 0o644).expect("create");
            let not_ea = fs.apply_create("/plain.bin", 0o644).expect("create");
            let subject = resolve(&fs, "/subject.txt").expect("resolve");
            plant_ea_inode_xattr(&fs, subject, "user.big", not_ea, &DECOY);
        }
        let fs = mount(&dev);
        let ino = resolve(&fs, "/subject.txt").expect("resolve");
        let (inode, raw) = fs.read_inode_verified(ino).expect("read inode");

        assert!(
            crate::xattr::read_all_resolved(&fs, &inode, &raw).is_err(),
            "control: the value behind this attribute cannot be read"
        );
        let names =
            crate::xattr::list_names(&fs, &inode, &raw).expect("listing names needs no value");
        assert!(names.iter().any(|n| n == "user.big"), "{names:?}");
        drop(fs);

        // And through the door the bug was reported at: the C entry point
        // must report the size, then write the name, instead of -1.
        let image =
            fs_ext4_test_support::temp_path!("fs_ext4_listxattr_{}.img", std::process::id());
        std::fs::write(&image, &*dev.bytes.lock().unwrap()).expect("write image");
        let c_image = std::ffi::CString::new(image.clone()).unwrap();
        let c_path = std::ffi::CString::new("/subject.txt").unwrap();
        unsafe {
            let handle = crate::capi::fs_ext4_mount(c_image.as_ptr());
            assert!(!handle.is_null(), "C mount");
            let needed =
                crate::capi::fs_ext4_listxattr(handle, c_path.as_ptr(), std::ptr::null_mut(), 0);
            assert!(needed > 0, "the probe returned {needed}");
            let mut buf = vec![0u8; needed as usize];
            let wrote = crate::capi::fs_ext4_listxattr(
                handle,
                c_path.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            );
            assert_eq!(wrote, needed);
            assert!(
                buf.split(|&b| b == 0).any(|n| n == b"user.big"),
                "{:?}",
                String::from_utf8_lossy(&buf)
            );
            crate::capi::fs_ext4_umount(handle);
        }
        let _ = std::fs::remove_file(&image);
    }

    /// The buffer-level parser cannot follow the pointer — it has no
    /// filesystem — so it must report the pointer and an EMPTY value
    /// rather than the bytes at `e_value_offs`. An empty value is a
    /// visible failure; a decoy is a plausible one.
    #[test]
    fn the_buffer_level_parser_reports_the_pointer_and_no_value() {
        let (dev, _real) = ea_inode_volume();
        let fs = mount(&dev);
        let ino = resolve(&fs, "/subject.txt").expect("resolve");
        let (inode, raw) = fs.read_inode_verified(ino).expect("read inode");

        let entries = crate::xattr::read_all(
            fs.dev.as_ref(),
            &inode,
            &raw,
            fs.sb.inode_size,
            fs.sb.block_size(),
        )
        .expect("read_all");
        let e = entries
            .iter()
            .find(|e| e.name == "user.big")
            .expect("the attribute is present");

        assert_ne!(e.value_inum, 0, "the pointer must be reported");
        assert!(
            e.value.is_empty(),
            "the value must not be taken from e_value_offs; got {:?}",
            e.value
        );
    }

    /// THE WRITE THAT WOULD ORPHAN IT. The in-inode region is rewritten
    /// wholesale, so touching any attribute re-emits every other one —
    /// and an EA-inode-backed entry cannot be re-emitted, because its
    /// value is not there to repack. Refusing is the honest answer while
    /// following and refcounting EA inodes is unimplemented.
    #[test]
    fn rewriting_an_attribute_area_holding_an_ea_inode_entry_is_refused() {
        let (dev, _real) = ea_inode_volume();
        let fs = mount(&dev);
        let ino = resolve(&fs, "/subject.txt").expect("resolve");
        let (_inode, mut raw) = fs.read_inode_verified(ino).expect("read inode");
        let inode_size = fs.sb.inode_size as usize;
        let extra_isize = u16::from_le_bytes(raw[128..130].try_into().unwrap()) as usize;
        let region = &mut raw[128 + extra_isize..inode_size];

        let removed = crate::xattr::plan_remove_in_inode_region(region, "user.big");
        assert!(
            matches!(removed, Err(Error::Unsupported(_))),
            "removing it must be refused, not silently orphan the EA inode; got {removed:?}"
        );

        let set = crate::xattr::plan_set_in_inode_region(region, "user.other", b"x");
        assert!(
            matches!(set, Err(Error::Unsupported(_))),
            "setting a DIFFERENT attribute must also be refused, because the rewrite \
             re-emits the EA-inode entry too; got {set:?}"
        );
    }

    /// The control: an ordinary inline attribute still reads and still
    /// rewrites, so none of the above can be satisfied by refusing
    /// everything.
    #[test]
    fn an_ordinary_inline_attribute_is_unaffected() {
        let dev = formatted();
        let fs = mount(&dev);
        let ino = fs.apply_create("/plain.txt", 0o644).expect("create");
        let (inode, mut raw) = fs.read_inode_verified(ino).expect("read inode");
        let inode_size = fs.sb.inode_size as usize;
        let extra_isize = u16::from_le_bytes(raw[128..130].try_into().unwrap()) as usize;
        {
            let region = &mut raw[128 + extra_isize..inode_size];
            crate::xattr::plan_set_in_inode_region(region, "user.small", b"hello")
                .expect("an ordinary set must still work");
        }
        let mut buf = BlockBuffer::new(fs.sb.block_size());
        fs.finalize_inode_raw(ino, inode.generation, &mut raw)
            .expect("finalize");
        fs.buffer_write_inode(&mut buf, ino, &raw).expect("write");
        fs.commit_block_buffer(buf).expect("commit");

        let (inode, raw) = fs.read_inode_verified(ino).expect("re-read");
        let got = crate::xattr::get_resolved(&fs, &inode, &raw, "user.small")
            .expect("get")
            .expect("present");
        assert_eq!(got, b"hello");
    }

    /// THE CASE THAT DESTROYED DATA. An inode on the orphan chain with a
    /// non-zero link count is a `truncate()` a crash interrupted. It is
    /// still named by its directory entries. Recovery must finish the
    /// truncate, not delete the file.
    #[test]
    fn an_orphan_that_still_has_links_is_truncated_not_deleted() {
        let dev = formatted();
        let payload: Vec<u8> = (0..4 * BS as usize).map(|i| (i % 251) as u8).collect();

        let ino = {
            let fs = mount(&dev);
            let ino = fs.apply_create("/keep.txt", 0o644).expect("create");
            fs.apply_pwrite("/keep.txt", 0, &payload).expect("write");
            fs.apply_link("/keep.txt", "/also-keep.txt").expect("link");
            ino
        };

        let before_free = {
            let fs = mount(&dev);
            // i_size drops to one block; the four blocks of extents stay.
            plant_orphan(&fs, ino, 2, Some(BS as u64));
            fs.sb.free_blocks_count
        };

        // Recovery runs on this mount; the next one observes the result.
        drop(mount(&dev));

        let fs = mount(&dev);
        assert!(
            fs.orphan_list().expect("orphan_list").is_empty(),
            "recovery must empty the chain"
        );

        let (inode, _) = fs.read_inode_verified(ino).expect("read inode");
        assert_eq!(
            inode.links_count, 2,
            "the file is named twice and was never unlinked; recovery deleted it"
        );
        assert_eq!(inode.dtime, 0, "a live file must not be stamped as deleted");
        assert_eq!(
            inode.size, BS as u64,
            "the interrupted truncate should be finished, not undone"
        );

        assert_eq!(resolve(&fs, "/keep.txt").expect("keep.txt"), ino);
        assert_eq!(resolve(&fs, "/also-keep.txt").expect("also-keep.txt"), ino);

        let data = crate::file_io::read_all(&fs, &inode).expect("read back");
        assert_eq!(
            data.len(),
            BS as usize,
            "the surviving file is its first i_size bytes"
        );
        assert_eq!(
            &data[..],
            &payload[..BS as usize],
            "and those bytes are the ones that were written"
        );

        assert_eq!(
            fs.sb.free_blocks_count,
            before_free + 3,
            "only the three blocks past the new EOF should have been freed"
        );
    }

    /// AN INTERRUPTED TRUNCATE THE PLANNER REFUSES STAYS ON THE CHAIN. A
    /// deep extent tree whose leaf fails its checksum cannot be planned.
    /// Recovery must leave the member at the head, untouched, and report
    /// no error -- the same answer it gives every member it cannot reclaim
    /// whole, so the members behind it and the previous member's pending
    /// `i_dtime` are handled the one way.
    #[test]
    fn an_interrupted_truncate_over_a_corrupt_deep_tree_stays_on_the_chain() {
        let dev = formatted();
        let ino = {
            let fs = mount(&dev);
            let ino = fs.apply_create("/deep.bin", 0o644).expect("create");
            // One block every other block: twelve extents, more than the
            // inline root's four, so the tree grows an external leaf.
            for i in 0..12u64 {
                fs.apply_pwrite("/deep.bin", i * 2 * BS as u64, &[0x5a; BS as usize])
                    .expect("write");
            }
            ino
        };

        let (leaf, size_before) = {
            let fs = mount(&dev);
            let (inode, _) = fs.read_inode_verified(ino).expect("read inode");
            assert!(
                crate::extent::ExtentHeader::parse(&inode.block)
                    .expect("root")
                    .depth
                    >= 1,
                "fixture: the tree is deeper than the inline root"
            );
            let leaf = crate::extent::ExtentIdx::parse(&inode.block[12..24])
                .expect("index")
                .leaf_block;
            plant_orphan(&fs, ino, 1, Some(BS as u64));
            (leaf, inode.size)
        };
        // Break the leaf's checksum: the tail's last byte.
        dev.bytes.lock().unwrap()[(leaf * BS as u64 + BS as u64 - 1) as usize] ^= 1;

        let fs = mount(&dev);
        assert_eq!(
            fs.recover_orphans()
                .expect("an unplannable member is not an error"),
            0
        );
        assert_eq!(
            fs.orphan_list().expect("orphan_list"),
            vec![ino],
            "the member it could not reclaim stays at the head"
        );
        let (inode, _) = fs.read_inode_verified(ino).expect("read inode");
        assert_eq!(inode.links_count, 1, "the file is still named");
        assert_eq!(inode.size, BS as u64, "its lowered size is kept");
        assert_ne!(size_before, inode.size, "fixture: the size was lowered");
    }

    /// A FILE WHOSE EXTENT LEAF FAILS ITS CHECKSUM IS NOT FREED AROUND.
    /// Freeing walks the tree to learn which blocks to release; a node that
    /// does not verify may name blocks that belong to something else. The
    /// kernel refuses the removal (EFSBADCRC); so does this, before any
    /// bitmap is touched.
    #[test]
    fn unlinking_a_file_whose_extent_leaf_fails_its_checksum_is_refused() {
        let dev = formatted();
        {
            let fs = mount(&dev);
            fs.apply_create("/deep.bin", 0o644).expect("create");
            for i in 0..12u64 {
                fs.apply_pwrite("/deep.bin", i * 2 * BS as u64, &[0x5a; BS as usize])
                    .expect("write");
            }
        }
        let leaf = {
            let fs = mount(&dev);
            let ino = resolve(&fs, "/deep.bin").expect("resolve");
            let (inode, _) = fs.read_inode_verified(ino).expect("read inode");
            crate::extent::ExtentIdx::parse(&inode.block[12..24])
                .expect("index")
                .leaf_block
        };
        dev.bytes.lock().unwrap()[(leaf * BS as u64 + BS as u64 - 1) as usize] ^= 1;

        let fs = mount(&dev);
        let free_before = fs.sb.free_blocks_count;
        let got = fs.apply_unlink("/deep.bin");
        assert!(
            matches!(got, Err(Error::BadChecksum { .. })),
            "unlink over an unverified extent leaf: {got:?}"
        );
        drop(fs);
        let fs = mount(&dev);
        assert!(resolve(&fs, "/deep.bin").is_ok(), "the file is still named");
        assert_eq!(fs.sb.free_blocks_count, free_before, "nothing was freed");
    }

    // --- i_file_acl: written where it is read -------------------------
    //
    // THE OBVIOUS TEST IS NOT ENOUGH, and this is the whole reason the
    // defect survived. A round trip on a small image passes today: below
    // 2^32 blocks both halves of `i_file_acl` are zero, the wrong offset
    // is clobbered with zero over zero, and nothing disagrees. The block
    // number has to have bits above 32 set.
    //
    // Driven against a synthetic 256-byte inode rather than a 16 TiB
    // filesystem, because what is being tested is which BYTES the two
    // writers touch, and that is answerable without the volume.

    /// A 256-byte inode with a plausible extra_isize, so `Inode::parse`
    /// reads it the way it reads a real one.
    fn synthetic_inode() -> Vec<u8> {
        let mut raw = vec![0u8; 256];
        raw[0x00..0x02].copy_from_slice(&0x81A4u16.to_le_bytes()); // S_IFREG | 0644
        raw[OFF_EXTRA_ISIZE..OFF_EXTRA_ISIZE + 2]
            .copy_from_slice(&EXTRA_ISIZE_DEFAULT.to_le_bytes());
        raw
    }

    /// THE DEFECT. A block number above 2^32 must survive the write.
    ///
    /// `patch_inode_size_and_blocks` runs after the splice at both real
    /// call sites and owns `0x74..0x76`, so it is run here too: with the
    /// old offset the high half was written and then immediately
    /// overwritten, and a test that skipped this call would have passed
    /// against the broken code.
    #[test]
    fn a_file_acl_block_above_2_32_survives_patch_inode_size_and_blocks() {
        let block: u64 = 0x0003_1234_5678; // bits above 32 set
        let mut raw = synthetic_inode();

        Filesystem::write_file_acl(&mut raw, block).expect("write_file_acl");
        Filesystem::patch_inode_size_and_blocks(&mut raw, 4096, 0x0000_0002_0000_0008)
            .expect("patch_inode_size_and_blocks");

        let inode = Inode::parse(&raw).expect("parse");
        assert_eq!(
            inode.file_acl, block,
            "i_file_acl read back as {:#x}, written as {:#x} — the high half is at \
             0x76..0x78 and the low at 0x68..0x6C",
            inode.file_acl, block
        );
    }

    /// THE FREE CONTROL the issue named: a fix that over-corrects by
    /// moving the wrong field fails here.
    ///
    /// `i_blocks_hi` still belongs to `patch_inode_size_and_blocks`, and
    /// the splice must not touch it. Without this, writing `i_file_acl_hi`
    /// to `0x74` and `i_blocks_hi` to `0x76` would satisfy the test above
    /// by symmetry.
    #[test]
    fn the_splice_leaves_i_blocks_hi_to_the_function_that_owns_it() {
        let mut raw = synthetic_inode();
        Filesystem::patch_inode_size_and_blocks(&mut raw, 4096, 0x0000_0002_0000_0008)
            .expect("patch");
        let blocks_hi_before = read_le16(&raw, 0x74);
        assert_eq!(
            blocks_hi_before, 2,
            "the fixture must set a nonzero blocks_hi"
        );

        Filesystem::write_file_acl(&mut raw, 0x0003_1234_5678).expect("write_file_acl");
        assert_eq!(
            read_le16(&raw, 0x74),
            blocks_hi_before,
            "the file_acl splice wrote i_blocks_hi (0x74..0x76), which belongs to \
             patch_inode_size_and_blocks"
        );
        assert_eq!(
            Inode::parse(&raw).expect("parse").blocks,
            0x0000_0002_0000_0008,
            "and i_blocks reads back unchanged"
        );
    }

    /// Clearing it clears BOTH halves. The free path zeroed only the low
    /// one, leaving `file_acl == old_hi << 32` — a nonzero pointer at a
    /// block just handed back to the allocator.
    #[test]
    fn clearing_file_acl_clears_the_high_half_too() {
        // THE STARTING STATE IS WRITTEN BY HAND, at the offsets the
        // on-disk format uses, because that is the inode this driver is
        // handed: one Linux or mkfs wrote with a real high half. Using
        // `write_file_acl` to set it up would make the test agree with
        // whatever offset that function happens to use, and a truncating
        // writer followed by a truncating clear reads back 0 either way.
        let mut raw = synthetic_inode();
        raw[0x68..0x6C].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        raw[0x76..0x78].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(
            Inode::parse(&raw).expect("parse").file_acl,
            0x0003_1234_5678,
            "the fixture must present an inode whose file_acl needs both halves"
        );

        Filesystem::write_file_acl(&mut raw, 0).expect("clear");
        Filesystem::patch_inode_size_and_blocks(&mut raw, 4096, 0x0000_0002_0000_0008)
            .expect("patch");
        assert_eq!(
            Inode::parse(&raw).expect("parse").file_acl,
            0,
            "a freed external xattr block must leave no pointer behind — clearing only \
             i_file_acl_lo leaves file_acl == old_hi << 32, at a block already handed \
             back to the allocator"
        );
    }

    /// The low half on its own still round-trips, so a volume under
    /// 2^32 blocks — every volume this has ever run on — is unaffected.
    #[test]
    fn a_small_file_acl_block_round_trips_as_it_always_did() {
        let mut raw = synthetic_inode();
        Filesystem::write_file_acl(&mut raw, 0x1234_5678).expect("write_file_acl");
        Filesystem::patch_inode_size_and_blocks(&mut raw, 4096, 8).expect("patch");
        assert_eq!(Inode::parse(&raw).expect("parse").file_acl, 0x1234_5678);
    }

    /// The length guard. `>= 0x76` admitted a buffer ending exactly where
    /// the field starts; and an inode that cannot hold the high half must
    /// refuse a block number that needs one rather than store a pointer
    /// to a different block.
    #[test]
    fn an_inode_too_short_for_the_high_half_refuses_a_block_that_needs_one() {
        let mut short = vec![0u8; 0x76];
        assert!(
            Filesystem::write_file_acl(&mut short, 0x0003_1234_5678).is_err(),
            "a 0x76-byte inode has no room for 0x76..0x78 and must not truncate"
        );
        assert_eq!(
            read_le32(&short, 0x68),
            0,
            "a refused write must not leave the truncated low half behind"
        );

        // ...but a block number that fits in 32 bits is fine there, which
        // is what makes the refusal a bound rather than a blanket no.
        let mut short = vec![0u8; 0x76];
        Filesystem::write_file_acl(&mut short, 0x1234_5678).expect("the low half fits");
        assert_eq!(read_le32(&short, 0x68), 0x1234_5678);

        let mut tiny = vec![0u8; 0x6B];
        assert!(
            Filesystem::write_file_acl(&mut tiny, 0).is_err(),
            "a buffer too short for even the low half must be refused"
        );
    }

    /// ONE RECIPE, AND THE TESTS ABOVE CANNOT SEE A SECOND ONE.
    ///
    /// Everything above drives `write_file_acl` directly. Re-inlining the
    /// splice at either call site — which is the state this file was in —
    /// leaves all of them green, because they never execute a call site.
    /// Reaching one needs a filesystem above 2^32 blocks, i.e. a 16 TiB
    /// image, which is not a test anyone will run.
    ///
    /// So this reads the source instead and requires each offset to be
    /// written in exactly one place: `0x74..0x76` only by
    /// `patch_inode_size_and_blocks`, which owns `i_blocks_hi`, and
    /// `0x76..0x78` only by `write_file_acl`. A hand-written second copy
    /// of either is what let the two disagree with `Inode::parse` for as
    /// long as they did.
    /// Every statement in `src` that writes `field` with `copy_from_slice`,
    /// whitespace collapsed.
    ///
    /// BY STATEMENT, NOT BY LINE (#162). rustfmt wraps a long write as
    /// `raw[0x74..0x76]` on one line and `.copy_from_slice(..)` on the next,
    /// so a line scan saw neither half as a write, and #157's defect could be
    /// re-inlined with fmt, clippy and this test all green. Line comments are
    /// removed first so a `;` or an offset in prose does not join or form a
    /// statement.
    fn half_word_writes(src: &str, field: &str) -> Vec<String> {
        let code: String = src
            .lines()
            .map(|line| line.find("//").map_or(line, |at| &line[..at]))
            .collect::<Vec<_>>()
            .join("\n");
        code.split(';')
            .map(|statement| statement.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|statement| statement.contains(field) && statement.contains("copy_from_slice"))
            .collect()
    }

    /// The guard's reader sees a write however rustfmt lays it out.
    #[test]
    fn the_half_word_scan_reads_a_write_rustfmt_wrapped() {
        let wrapped = "
                    let acl_lo_value_for_the_external_xattr_block: u32 = 0;
                    raw[0x74..0x76]
                        .copy_from_slice(&acl_hi_value_for_the_external_xattr_block.to_le_bytes());
                    // raw[0x74..0x76].copy_from_slice(&in_a_comment);
        ";
        assert_eq!(half_word_writes(wrapped, "0x74..0x76").len(), 1);
        let one_line = "raw[0x74..0x76].copy_from_slice(&x.to_le_bytes());";
        assert_eq!(half_word_writes(one_line, "0x74..0x76").len(), 1);
        assert!(half_word_writes("let a = raw[0x74..0x76][0];", "0x74..0x76").is_empty());
    }

    #[test]
    fn each_inode_half_word_is_written_in_exactly_one_place() {
        let whole = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/fs.rs"))
            .expect("src/fs.rs is readable");

        // ONLY THE SHIPPING HALF. This module writes those offsets by hand
        // to build fixtures, and counting its own fixtures as writers made
        // the check fail the moment a test was added — which is a guard
        // that reports a defect it has manufactured. The split is on the
        // `#[cfg(test)]` that begins this module.
        let cut = whole
            .find("\n#[cfg(test)]\n")
            .expect("src/fs.rs has a #[cfg(test)] module, which is where this test lives");
        let src = &whole[..cut];
        assert!(
            src.contains("fn patch_inode_size_and_blocks"),
            "the non-test half must still contain the writers, or a count of 1 means \
             the split ate the file"
        );

        let writes = |field: &str| half_word_writes(src, field);

        let blocks_hi = writes("0x74..0x76");
        assert_eq!(
            blocks_hi.len(),
            1,
            "i_blocks_hi (0x74..0x76) is written {} times; it belongs to \
             patch_inode_size_and_blocks alone. Found: {blocks_hi:?}",
            blocks_hi.len()
        );
        assert!(
            blocks_hi[0].contains("blocks_hi"),
            "the one 0x74..0x76 write must be the i_blocks_hi one: {:?}",
            blocks_hi[0]
        );

        let file_acl_hi = writes("0x76..0x78");
        assert_eq!(
            file_acl_hi.len(),
            1,
            "i_file_acl_hi (0x76..0x78) is written {} times; write_file_acl is the one \
             place. Found: {file_acl_hi:?}",
            file_acl_hi.len()
        );

        // The control: this reader can see a write at all, so a count of
        // 1 means one and not a pattern that matches nothing.
        let file_acl_lo = writes("0x68..0x6C");
        assert_eq!(
            file_acl_lo.len(),
            1,
            "i_file_acl_lo (0x68..0x6C) should also be written exactly once, by the same \
             function. Found: {file_acl_lo:?}"
        );
    }

    /// Make the regular file `ino` an inline-data file whose 4 content
    /// bytes, held in `i_block`, are `content` — the way the kernel keeps a
    /// file that fits in the inode.
    fn make_inline_file(fs: &Filesystem, ino: u32, content: u32) {
        let (inode, mut raw) = fs.read_inode_verified(ino).unwrap();
        let mut flags = u32::from_le_bytes(raw[0x20..0x24].try_into().unwrap());
        flags &= !crate::inode::InodeFlags::EXTENTS.bits();
        flags |= crate::inode::InodeFlags::INLINE_DATA.bits();
        raw[0x20..0x24].copy_from_slice(&flags.to_le_bytes());
        raw[0x28..0x64].fill(0);
        raw[0x28..0x2C].copy_from_slice(&content.to_le_bytes());
        raw[0x04..0x08].copy_from_slice(&4u32.to_le_bytes());
        fs.finalize_inode_raw(ino, inode.generation, &mut raw)
            .unwrap();
        fs.write_inode_raw(ino, &raw).unwrap();
    }

    /// Whether `block` is marked in use in its group's bitmap.
    fn block_in_use(fs: &Filesystem, block: u64) -> bool {
        let rel = block - fs.sb.first_data_block as u64;
        let gi = (rel / fs.sb.blocks_per_group as u64) as usize;
        let bit = (rel % fs.sb.blocks_per_group as u64) as usize;
        let bm = fs.read_block(fs.groups[gi].block_bitmap).unwrap();
        bm[bit / 8] & (1 << (bit % 8)) != 0
    }

    /// #383: replacing the content of an inline-data file must not read its
    /// data bytes as block pointers and free them. Since #428 the replace
    /// goes through, in the inode.
    #[test]
    fn replacing_an_inline_file_does_not_free_its_bytes_as_blocks() {
        let dev = formatted();
        set_incompat_bit(&dev, crate::features::Incompat::INLINE_DATA.bits());
        let fs = mount(&dev);
        let ino = fs.apply_create("/f", 0o644).unwrap();
        let (root, _) = fs.read_inode_verified(2).unwrap();
        let victim = fs.map_inode_logical(&root, 0).unwrap().unwrap(); // root dir's block
        make_inline_file(&fs, ino, victim as u32);
        drop(fs);
        let fs = mount(&dev);
        let r = fs.apply_replace_file_content("/f", b"new content");
        drop(fs);
        let fs = mount(&dev);
        assert!(
            block_in_use(&fs, victim),
            "root directory block {victim} was freed: {r:?}"
        );
        assert_eq!(r.unwrap(), 11);
        let (inode, raw) = fs.read_inode_verified(ino).unwrap();
        assert!(inode.has_inline_data(), "11 bytes fit in the inode");
        assert_eq!(
            crate::file_io::read_inline(&fs, &inode, &raw).unwrap(),
            b"new content"
        );
    }

    /// The inline file's bytes and whether it is still inline.
    fn file_bytes(fs: &Filesystem, ino: u32) -> (Vec<u8>, bool) {
        let (inode, raw) = fs.read_inode_verified(ino).unwrap();
        let mut out = vec![0u8; inode.size as usize];
        let n = crate::file_io::read_with_raw(fs, &inode, &raw, 0, inode.size, &mut out).unwrap();
        assert_eq!(n, inode.size);
        (out, inode.has_inline_data())
    }

    /// #428: truncate and pwrite of an inline-data file are made in the
    /// inode while the result fits (a grow used to patch only `i_size`,
    /// past what the inline area holds, #383), and convert the file to an
    /// extent-mapped one when it does not; the other files are untouched
    /// and the volume audits clean after each step.
    #[test]
    fn truncating_or_writing_an_inline_file_keeps_it_inline_while_it_fits() {
        let dev = formatted();
        set_incompat_bit(&dev, crate::features::Incompat::INLINE_DATA.bits());
        let fs = mount(&dev);
        let ino = fs.apply_create("/f", 0o644).unwrap();
        fs.apply_create("/g", 0o644).unwrap();
        fs.apply_replace_file_content("/g", b"neighbour").unwrap();
        make_inline_file(&fs, ino, u32::from_le_bytes(*b"abc\n"));
        drop(fs);

        let mut model = b"abc\n".to_vec();
        type Step = fn(&Filesystem, u32) -> Result<()>;
        type Model = fn(&mut Vec<u8>);
        let steps: [(&str, Step, Model, bool); 6] = [
            (
                "pwrite",
                |fs, _| fs.apply_pwrite("/f", 1, b"x").map(drop),
                |m| m[1] = b'x',
                true,
            ),
            (
                "grow into system.data",
                |fs, ino| fs.apply_truncate_grow(ino, 100),
                |m| m.resize(100, 0),
                true,
            ),
            (
                "pwrite in system.data",
                |fs, ino| fs.apply_pwrite_ino(ino, 90, b"tail").map(drop),
                |m| m[90..94].copy_from_slice(b"tail"),
                true,
            ),
            (
                "shrink",
                |fs, ino| fs.apply_truncate_shrink(ino, 2),
                |m| m.truncate(2),
                true,
            ),
            (
                "grow past the inode",
                |fs, ino| fs.apply_truncate_grow(ino, 4096),
                |m| m.resize(4096, 0),
                false,
            ),
            (
                "pwrite the converted file",
                |fs, ino| fs.apply_pwrite_ino(ino, 4000, b"end").map(drop),
                |m| m[4000..4003].copy_from_slice(b"end"),
                false,
            ),
        ];
        for (name, step, apply, inline) in steps {
            let fs = mount(&dev);
            step(&fs, ino).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            drop(fs);
            apply(&mut model);
            let fs = mount(&dev);
            assert_eq!(file_bytes(&fs, ino), (model.clone(), inline), "{name}");
            let g = resolve(&fs, "/g").unwrap();
            assert_eq!(file_bytes(&fs, g).0, b"neighbour", "{name}: /g");
            let report = crate::fsck::audit(&fs, u32::MAX, u32::MAX).unwrap();
            assert!(
                report.anomalies.is_empty(),
                "{name}: {:?}",
                report.anomalies
            );
        }
    }

    /// Tests that run an oracle tool (they run in the harness VM): mkfs.ext4, e2fsck.
    mod needs_host {
        use super::*;

        /// #384, judged by e2fsck: recovering an orphaned fast symlink that
        /// holds an xattr block frees the inode and the xattr block and
        /// nothing its target text names -- a freed in-use block is what
        /// e2fsck's pass 5 reports.
        #[test]
        fn recovering_an_orphaned_fast_symlink_with_an_xattr_block_leaves_e2fsck_clean() {
            let dev = formatted();
            {
                let fs = mount(&dev);
                let (root, _) = fs.read_inode_verified(2).unwrap();
                let victim = fs.map_inode_logical(&root, 0).unwrap().unwrap();
                let mut tb: Vec<u8> = (victim as u32).to_le_bytes().to_vec();
                while tb.last() == Some(&0) {
                    tb.pop();
                }
                let target = String::from_utf8(tb).expect("an ASCII block number");
                let ino = fs.apply_symlink(&target, "/l").unwrap();
                give_xattr_block(&fs, ino);
                let (root, _) = fs.read_inode_verified(2).unwrap();
                let mut buf = BlockBuffer::new(fs.sb.block_size());
                fs.buffer_remove_dir_entry(&mut buf, 2, &root, b"l")
                    .expect("remove the name");
                fs.commit_block_buffer(buf).expect("commit");
                plant_orphan(&fs, ino, 0, None);
            }
            // Recovery runs on this mount.
            drop(mount(&dev));

            let image = fs_ext4_test_support::temp_dir()
                .join(format!("fs_ext4_symlink_orphan_{}.img", std::process::id()));
            std::fs::write(&image, &*dev.bytes.lock().unwrap()).unwrap();
            let judged = fs_ext4_test_support::oracle("e2fsck")
                .arg("-fn")
                .arg(&image)
                .judged();
            let _ = std::fs::remove_file(&image);
            judged.clean("an orphaned fast symlink with an xattr block, recovered");
        }

        /// A run reaching past `blocks_count` into the short final group's
        /// padding is refused; one ending at the last block is not.
        #[test]
        fn a_run_into_the_final_groups_padding_is_refused() {
            let dir = fs_ext4_test_support::temp_dir()
                .join(format!("ext4-short-group-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let img = dir.join("s.img");
            // 16284 blocks at 4096 per group: the last group has 4 blocks and
            // 4092 bits of padding.
            std::fs::File::create(&img)
                .unwrap()
                .set_len(16284 * 4096)
                .unwrap();
            let made = fs_ext4_test_support::oracle("mkfs.ext4")
                .args(["-q", "-F", "-b", "4096", "-g", "4096", "-O", "^has_journal"])
                .arg(&img)
                .output();
            assert!(
                made.status.success(),
                "{}",
                String::from_utf8_lossy(&made.stderr)
            );
            let fs = Filesystem::mount(std::sync::Arc::new(
                crate::block_io::FileDevice::open(img.to_str().unwrap()).unwrap(),
            ))
            .unwrap();
            let last = fs.sb.blocks_count;
            assert_ne!(
                (last - u64::from(fs.sb.first_data_block)) % u64::from(fs.sb.blocks_per_group),
                0,
                "fixture: the final group is short"
            );
            assert!(
                fs.group_chunks(last - 2, 2).is_ok(),
                "a run ending at the last block"
            );
            assert!(
                matches!(fs.group_chunks(last - 2, 3), Err(Error::InvalidBlock(_))),
                "a run one block into the padding"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A freed run that crosses a group boundary credits each group its
        /// own blocks and clears the second group's bits (#118).
        ///
        /// Witnessed by the descriptors and the second group's bitmap, not the
        /// return value or the superblock delta: those are `len` before and
        /// after the fix alike. Measured before it: group 1 credited 8 and
        /// group 2 credited 0, its four bits left set. Fails without mkfs.ext4
        /// (they run in the harness VM).
        #[test]
        fn a_freed_run_across_a_group_boundary_credits_both_groups() {
            let dir = fs_ext4_test_support::temp_dir()
                .join(format!("ext4-cross-group-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let img = dir.join("g.img");
            std::fs::File::create(&img)
                .unwrap()
                .set_len(64 * 1024 * 1024)
                .unwrap();
            let made = fs_ext4_test_support::oracle("mkfs.ext4")
                .args([
                    "-q",
                    "-F",
                    "-b",
                    "4096",
                    "-g",
                    "4096",
                    "-O",
                    "^metadata_csum,^has_journal",
                ])
                .arg(&img)
                .output();
            assert!(
                made.status.success(),
                "{}",
                String::from_utf8_lossy(&made.stderr)
            );
            let path = img.to_str().unwrap().to_owned();
            let mount = || {
                Filesystem::mount(std::sync::Arc::new(
                    crate::block_io::FileDevice::open_rw(&path).unwrap(),
                ))
                .unwrap()
            };
            let boundary = 2 * 4096u64; // group 2 starts here
            let bit_set = |fs: &Filesystem, gi: usize, bit: u64| {
                let bm = fs.read_block(fs.groups[gi].block_bitmap).unwrap();
                bm[(bit / 8) as usize] & (1 << (bit % 8)) != 0
            };

            // Four blocks either side of the boundary, allocated per group.
            {
                let fs = mount();
                let mut buf = BlockBuffer::new(fs.sb.block_size());
                fs.buffer_mark_block_run_used(&mut buf, boundary - 4, 4)
                    .unwrap();
                fs.buffer_mark_block_run_used(&mut buf, boundary, 4)
                    .unwrap();
                fs.commit_block_buffer(buf).unwrap();
            }
            let (free1, free2) = {
                let fs = mount();
                assert!(
                    (0..4).all(|b| bit_set(&fs, 2, b)),
                    "fixture: group 2's run is allocated"
                );
                (
                    fs.groups[1].free_blocks_count,
                    fs.groups[2].free_blocks_count,
                )
            };

            // One run across the boundary, as a merged extent frees it.
            {
                let fs = mount();
                let mut buf = BlockBuffer::new(fs.sb.block_size());
                assert_eq!(
                    fs.buffer_free_block_run_and_bgd(&mut buf, boundary - 4, 8)
                        .unwrap(),
                    8
                );
                fs.commit_block_buffer(buf).unwrap();
            }
            let fs = mount();
            assert_eq!(
                (
                    fs.groups[1].free_blocks_count - free1,
                    fs.groups[2].free_blocks_count - free2
                ),
                (4, 4),
                "(group 1 credited, group 2 credited)"
            );
            assert!(
                (0..4).all(|b| !bit_set(&fs, 2, b)),
                "group 2's blocks are still marked allocated"
            );
            drop(fs);

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// An ext2 orphan's data blocks and indirect blocks go back to the
        /// free pool with it (#79).
        ///
        /// The file is 20 blocks at 4 KiB, so its tree has a single indirect
        /// block beside the twelve direct pointers. Recovery used to free the
        /// inode and none of its blocks, and since #201 declined such an
        /// orphan altogether rather than strand them; either way the blocks
        /// stayed allocated. After the fix the free count returns to what it
        /// was before the file was written, and fsck finds nothing.
        #[test]
        fn an_indirect_mapped_orphan_frees_its_data_and_indirect_blocks() {
            const EXT2_VOL: u64 = 32 * 1024 * 1024;
            let dev = MemDev::new(EXT2_VOL);
            crate::mkfs::format_filesystem_with_flavor(
                dev.as_ref(),
                Some("ext2orph"),
                None,
                EXT2_VOL,
                BS,
                crate::features::FsFlavor::Ext2,
            )
            .expect("format");
            let (ino, free_before) = {
                let fs = mount(&dev);
                let free_before = fs.sb.free_blocks_count;
                let ino = fs.apply_create("/gone.bin", 0o644).expect("create");
                fs.apply_replace_file_content("/gone.bin", &vec![0x5A; 20 * BS as usize])
                    .expect("write");
                let (inode, _) = fs.read_inode_verified(ino).expect("read");
                assert!(!inode.has_extents(), "fixture: an indirect-mapped file");
                assert_ne!(
                    u32::from_le_bytes(inode.block[48..52].try_into().unwrap()),
                    0,
                    "fixture: the file uses its single indirect block"
                );
                (ino, free_before)
            };
            {
                let fs = mount(&dev);
                let (root, _) = fs.read_inode_verified(2).expect("root");
                let mut buf = BlockBuffer::new(fs.sb.block_size());
                fs.buffer_remove_dir_entry(&mut buf, 2, &root, b"gone.bin")
                    .expect("remove the name");
                fs.commit_block_buffer(buf).expect("commit");
                plant_orphan(&fs, ino, 0, None);
            }
            // Recovery runs on this mount; the next one observes the result.
            drop(mount(&dev));

            let fs = mount(&dev);
            assert!(
                fs.orphan_list().expect("orphan_list").is_empty(),
                "recovery must take the orphan off the chain"
            );
            assert!(!fs.inode_bit_is_set(ino).expect("inode bitmap"));
            assert_eq!(
                fs.sb.free_blocks_count, free_before,
                "every block the file used -- 20 data and 1 indirect -- must be free again"
            );
            let report = crate::fsck::audit(&fs, u32::MAX, u32::MAX).expect("audit");
            assert!(report.is_clean(), "fsck: {:?}", report.anomalies);
            drop(fs);

            // And e2fsck: a block left allocated with nothing mapping it is
            // exactly what its pass 5 reports.
            let image = fs_ext4_test_support::temp_dir().join(format!(
                "fs_ext4_indirect_orphan_{}.img",
                std::process::id()
            ));
            std::fs::write(&image, &*dev.bytes.lock().unwrap()).unwrap();
            let judged = fs_ext4_test_support::oracle("e2fsck")
                .arg("-fn")
                .arg(&image)
                .judged();
            let _ = std::fs::remove_file(&image);
            judged.clean("an indirect-mapped orphan after its data is freed");
        }
    }

    /// #391: fsck counts an uninit group the way the format defines it —
    /// a BLOCK_UNINIT group's blocks from its own metadata, an INODE_UNINIT
    /// group's inodes as all free — not from bitmap blocks the format leaves
    /// unspecified. Group 2 of a 2 KiB-block volume (the smallest block that
    /// formats more than one group) holds nothing but its own bitmaps and
    /// inode table, so flagging it uninit is honest.
    #[test]
    fn fsck_does_not_count_an_uninit_groups_bitmaps() {
        const SMALL_BS: u32 = 2048;
        // Two 32 MiB groups and a partial third.
        const SMALL_VOL: u64 = 80 * 1024 * 1024;
        let dev = MemDev::new(SMALL_VOL);
        crate::mkfs::format_filesystem(dev.as_ref(), Some("uninit"), None, SMALL_VOL, SMALL_BS)
            .expect("format");
        let gi = 2usize;
        {
            let fs = mount(&dev);
            assert!(fs.groups.len() > gi, "the volume needs a third group");
            let g = fs.groups[gi];
            assert_eq!(g.free_inodes_count, fs.sb.inodes_per_group);
            assert_eq!(g.used_dirs_count, 0);
            let (blk, off) = fs.sb.descriptor_location(gi as u64);
            let mut raw = fs.read_block(blk).unwrap();
            let flags = u16::from_le_bytes(raw[off + 0x12..off + 0x14].try_into().unwrap())
                | crate::bgd::BgdFlags::BLOCK_UNINIT.bits()
                | crate::bgd::BgdFlags::INODE_UNINIT.bits();
            raw[off + 0x12..off + 0x14].copy_from_slice(&flags.to_le_bytes());
            fs.restamp_group_desc_csum(&mut raw[..], off, gi);
            dev.write_at(blk * u64::from(SMALL_BS), &raw).unwrap();
        }
        {
            let fs = mount(&dev);
            let before = fs.audit(1000, 10000).unwrap();
            assert!(
                before.is_clean(),
                "precondition: clean with the real bitmaps still in place: {before:?}"
            );
            let g = fs.groups[gi];
            let bs = SMALL_BS as usize;
            dev.write_at(g.block_bitmap * u64::from(SMALL_BS), &vec![0u8; bs])
                .unwrap();
            dev.write_at(g.inode_bitmap * u64::from(SMALL_BS), &vec![0xFFu8; bs])
                .unwrap();
        }
        let fs = mount(&dev);
        let r = fs.audit(1000, 10000).unwrap();
        assert!(
            r.is_clean(),
            "an uninit group's bitmap bytes were counted: {r:?}"
        );
    }

    /// #390: the buffered BGD counter patch writes each counter's high half
    /// where `BlockGroupDescriptor::parse` reads it (free_blocks_hi 0x2C,
    /// free_inodes_hi 0x2E, used_dirs_hi 0x30), so a carry out of the low
    /// half lands in the counter and not in `bg_inode_table_hi`.
    #[test]
    fn buffered_bgd_counter_high_halves_are_written_where_they_are_read() {
        let dev = formatted();
        let fs = mount(&dev);
        assert!(fs.sb.desc_size >= 64, "the fixture must carry high halves");
        let (blk, off) = fs.sb.descriptor_location(0);
        let mut buf = BlockBuffer::new(BS);
        {
            let b = buf.get_mut(&fs, blk).unwrap();
            b[off + 0x0C..off + 0x0E].copy_from_slice(&0xFFFFu16.to_le_bytes());
            b[off + 0x0E..off + 0x10].copy_from_slice(&0xFFFFu16.to_le_bytes());
            b[off + 0x10..off + 0x12].copy_from_slice(&0xFFFFu16.to_le_bytes());
        }
        let parse = |buf: &BlockBuffer| {
            crate::bgd::BlockGroupDescriptor::parse(&buf.dirty[&blk][off..off + 64], 64).unwrap()
        };
        let before = parse(&buf);
        fs.buffer_patch_bgd_counters(&mut buf, 0, 1, 1, 1).unwrap();
        let after = parse(&buf);
        assert_eq!(
            after.inode_table, before.inode_table,
            "inode table pointer changed"
        );
        assert_eq!(after.block_bitmap, before.block_bitmap);
        assert_eq!(after.inode_bitmap, before.inode_bitmap);
        assert_eq!(after.free_blocks_count, 0x1_0000);
        assert_eq!(after.free_inodes_count, 0x1_0000);
        assert_eq!(after.used_dirs_count, 0x1_0000);
        assert_eq!(after.itable_unused, before.itable_unused);

        // And a borrow back across the boundary returns every field.
        fs.buffer_patch_bgd_counters(&mut buf, 0, -1, -1, -1)
            .unwrap();
        let back = parse(&buf);
        assert_eq!(back.inode_table, before.inode_table);
        assert_eq!(back.free_blocks_count, 0xFFFF);
        assert_eq!(back.free_inodes_count, 0xFFFF);
        assert_eq!(back.used_dirs_count, 0xFFFF);
    }

    /// #390: the unbuffered BGD counter patch (the one fsck repair uses)
    /// agrees with `parse` on where the high halves live.
    #[test]
    fn direct_bgd_counter_high_halves_are_written_where_they_are_read() {
        let dev = formatted();
        let fs = mount(&dev);
        assert!(fs.sb.desc_size >= 64, "the fixture must carry high halves");
        let (blk, off) = fs.sb.descriptor_location(0);
        let mut raw = fs.read_block(blk).unwrap();
        raw[off + 0x0C..off + 0x0E].copy_from_slice(&0xFFFFu16.to_le_bytes());
        raw[off + 0x0E..off + 0x10].copy_from_slice(&0xFFFFu16.to_le_bytes());
        raw[off + 0x10..off + 0x12].copy_from_slice(&0xFFFFu16.to_le_bytes());
        fs.restamp_group_desc_csum(&mut raw[..], off, 0);
        dev.write_at(blk * u64::from(BS), &raw).unwrap();
        drop(fs);
        let fs = mount(&dev);
        let before = crate::bgd::BlockGroupDescriptor::parse(&raw[off..off + 64], 64).unwrap();
        fs.patch_bgd_counters(0, 1, 1, 1).unwrap();
        let raw = fs.read_block(blk).unwrap();
        let after = crate::bgd::BlockGroupDescriptor::parse(&raw[off..off + 64], 64).unwrap();
        assert_eq!(
            after.inode_table, before.inode_table,
            "inode table pointer changed"
        );
        assert_eq!(after.free_blocks_count, 0x1_0000);
        assert_eq!(after.free_inodes_count, 0x1_0000);
        assert_eq!(after.used_dirs_count, 0x1_0000);
    }

    // --- one copy of an attribute, wherever it lives (#377) -----------

    /// The value length of every copy of `name` on `ino`, in the order the
    /// reader finds them: in-inode first, then the external block.
    fn xattr_copies(fs: &Filesystem, ino: u32, name: &str) -> Vec<usize> {
        let (inode, raw) = fs.read_inode_verified(ino).expect("read inode");
        crate::xattr::read_all_resolved(fs, &inode, &raw)
            .expect("read xattrs")
            .into_iter()
            .filter(|e| e.name == name)
            .map(|e| e.value.len())
            .collect()
    }

    /// Replacing an in-inode attribute with a value too big for the inode
    /// leaves exactly one copy, holding the new value. The in-inode copy
    /// used to stay, and the reader, which returns the first match, went
    /// on answering with the old value.
    #[test]
    fn growing_an_in_inode_attribute_past_the_inode_leaves_one_copy() {
        let dev = formatted();
        let fs = mount(&dev);
        let ino = fs.apply_create("/f", 0o644).expect("create");
        fs.apply_setxattr("/f", "user.a", &[1u8; 40])
            .expect("set small");
        assert_eq!(xattr_copies(&fs, ino, "user.a"), vec![40]);
        fs.apply_setxattr("/f", "user.a", &[2u8; 200])
            .expect("set big");
        assert_eq!(
            xattr_copies(&fs, ino, "user.a"),
            vec![200],
            "one copy, the new value"
        );
        let (inode, raw) = fs.read_inode_verified(ino).unwrap();
        assert_eq!(
            crate::xattr::get_resolved(&fs, &inode, &raw, "user.a").unwrap(),
            Some(vec![2u8; 200]),
            "the value read back is the one written last"
        );
        // Removing it removes it.
        fs.apply_removexattr("/f", "user.a").expect("remove");
        assert_eq!(xattr_copies(&fs, ino, "user.a"), Vec::<usize>::new());
    }

    /// Replacing an attribute that lives in the external block with a value
    /// that now fits in the inode does not leave the block's copy behind,
    /// and a remove then leaves no copy anywhere. The block's copy used to
    /// stay, and came back as the value once the in-inode one was removed.
    #[test]
    fn shrinking_an_external_attribute_into_the_inode_leaves_one_copy() {
        let dev = formatted();
        let fs = mount(&dev);
        let ino = fs.apply_create("/f", 0o644).expect("create");
        fs.apply_setxattr("/f", "user.big", &[1u8; 200])
            .expect("set big");
        assert_eq!(xattr_copies(&fs, ino, "user.big"), vec![200]);
        fs.apply_setxattr("/f", "user.big", &[2u8; 8])
            .expect("set small");
        assert_eq!(
            xattr_copies(&fs, ino, "user.big"),
            vec![8],
            "one copy, the new value"
        );
        fs.apply_removexattr("/f", "user.big").expect("remove");
        assert_eq!(
            xattr_copies(&fs, ino, "user.big"),
            Vec::<usize>::new(),
            "removed means gone"
        );
    }

    // --- the external xattr block is checked before it is edited (#378) ---

    /// Point `f`'s `i_file_acl` at `block_nr`, re-checksummed, as a stale
    /// or bit-rotted pointer would leave it.
    fn point_file_acl_at(fs: &Filesystem, ino: u32, block_nr: u64) {
        let (inode, mut raw) = fs.read_inode_verified(ino).expect("read inode");
        Filesystem::write_file_acl(&mut raw, block_nr).expect("write_file_acl");
        fs.finalize_inode_raw(ino, inode.generation, &mut raw)
            .expect("finalize");
        fs.write_inode_raw(ino, &raw).expect("write inode");
    }

    /// `/g`, one block of 0x5A, and the number of that block.
    fn a_data_block(fs: &Filesystem) -> u64 {
        let g = fs.apply_create("/g", 0o644).expect("create g");
        fs.apply_replace_file_content("/g", &[0x5Au8; 4096])
            .expect("write g");
        let (gi, _) = fs.read_inode_verified(g).expect("read g");
        fs.extent_tree_runs(g, &gi).expect("g's extents")[0].0
    }

    fn block_bytes(dev: &std::sync::Arc<MemDev>, block_nr: u64) -> Vec<u8> {
        use crate::block_io::BlockDevice;
        let mut b = vec![0u8; BS as usize];
        dev.read_at(block_nr * BS as u64, &mut b)
            .expect("read block");
        b
    }

    /// An `i_file_acl` naming a block without the xattr magic does not name
    /// an xattr block: setxattr refuses it rather than formatting another
    /// file's data as one and reporting success.
    #[test]
    fn setxattr_refuses_an_external_block_without_the_xattr_magic() {
        let dev = formatted();
        let fs = mount(&dev);
        let f = fs.apply_create("/f", 0o644).expect("create f");
        let gblk = a_data_block(&fs);
        point_file_acl_at(&fs, f, gblk);
        let r = fs.apply_setxattr("/f", "user.big", &[1u8; 200]);
        assert!(
            matches!(r, Err(Error::Corrupt(_))),
            "setxattr on a non-xattr block must be refused as corrupt, got {r:?}"
        );
        assert!(
            block_bytes(&dev, gblk).iter().all(|&x| x == 0x5A),
            "g's data must be untouched"
        );
    }

    /// The same for removexattr and for the release an unlink does: neither
    /// edits nor frees a block that is not an xattr block.
    #[test]
    fn removexattr_and_unlink_refuse_an_external_block_without_the_xattr_magic() {
        let dev = formatted();
        let fs = mount(&dev);
        let f = fs.apply_create("/f", 0o644).expect("create f");
        let gblk = a_data_block(&fs);
        point_file_acl_at(&fs, f, gblk);
        let r = fs.apply_removexattr("/f", "user.anything");
        assert!(matches!(r, Err(Error::Corrupt(_))), "removexattr: {r:?}");
        let r = fs.apply_unlink("/f");
        assert!(matches!(r, Err(Error::Corrupt(_))), "unlink: {r:?}");
        assert!(
            block_bytes(&dev, gblk).iter().all(|&x| x == 0x5A),
            "g's data must be untouched"
        );
    }

    /// A metadata_csum xattr block whose checksum does not verify is not
    /// edited, and so not restamped as if it did.
    #[test]
    fn setxattr_refuses_an_external_block_that_fails_its_checksum() {
        use crate::block_io::BlockDevice;
        let dev = formatted();
        let fs = mount(&dev);
        assert!(fs.csum.enabled, "fixture: a metadata_csum volume");
        let f = fs.apply_create("/f", 0o644).expect("create f");
        fs.apply_setxattr("/f", "user.big", &[1u8; 200])
            .expect("set big");
        let (fi, _) = fs.read_inode_verified(f).unwrap();
        assert_ne!(fi.file_acl, 0, "fixture: the value is in the block");
        let mut b = block_bytes(&dev, fi.file_acl);
        assert!(
            fs.csum.verify_xattr_block(fi.file_acl, &b),
            "fixture: valid"
        );
        b[BS as usize - 1] ^= 0xFF; // a byte of the value area
        dev.write_at(fi.file_acl * BS as u64, &b).unwrap();
        let fs = mount(&dev);
        let r = fs.apply_setxattr("/f", "user.other", &[2u8; 200]);
        assert!(
            matches!(r, Err(Error::BadChecksum { .. })),
            "an edit of a block that fails its checksum must be refused, got {r:?}"
        );
        assert_eq!(block_bytes(&dev, fi.file_acl), b, "the block is untouched");
        let r = fs.apply_removexattr("/f", "user.big");
        assert!(
            matches!(r, Err(Error::BadChecksum { .. })),
            "removexattr: {r:?}"
        );
        assert_eq!(block_bytes(&dev, fi.file_acl), b, "the block is untouched");
        // The read path says so too, rather than returning what may be
        // another attribute's bytes.
        let (fi, raw) = fs.read_inode_verified(f).unwrap();
        let r = crate::xattr::get_resolved(&fs, &fi, &raw, "user.big");
        assert!(
            matches!(r, Err(Error::BadChecksum { .. })),
            "getxattr: {r:?}"
        );
    }

    /// `h_blocks` is 1 for every xattr block ext4 has ever written; a block
    /// claiming more is not one this crate or the kernel can edit.
    #[test]
    fn setxattr_refuses_an_external_block_with_h_blocks_other_than_one() {
        use crate::block_io::BlockDevice;
        let dev = formatted();
        let fs = mount(&dev);
        let f = fs.apply_create("/f", 0o644).expect("create f");
        fs.apply_setxattr("/f", "user.big", &[1u8; 200])
            .expect("set big");
        let (fi, _) = fs.read_inode_verified(f).unwrap();
        let mut b = block_bytes(&dev, fi.file_acl);
        b[8..12].copy_from_slice(&2u32.to_le_bytes());
        fs.csum.patch_xattr_block(fi.file_acl, &mut b);
        dev.write_at(fi.file_acl * BS as u64, &b).unwrap();
        let fs = mount(&dev);
        let r = fs.apply_setxattr("/f", "user.other", &[2u8; 200]);
        assert!(matches!(r, Err(Error::Corrupt(_))), "setxattr: {r:?}");
        assert_eq!(block_bytes(&dev, fi.file_acl), b, "the block is untouched");
    }

    // --- an inode with i_extra_isize = 0 (#380) ------------------------

    /// Rewrite `ino`'s raw image through `f`, re-checksummed.
    fn patch_inode(fs: &Filesystem, ino: u32, f: impl FnOnce(&mut Vec<u8>)) {
        let (inode, mut raw) = fs.read_inode_verified(ino).expect("read inode");
        f(&mut raw);
        fs.finalize_inode_raw(ino, inode.generation, &mut raw)
            .expect("finalize");
        fs.write_inode_raw(ino, &raw).expect("write inode");
    }

    /// With `i_extra_isize = 0` the kernel parses no in-inode xattr area
    /// and reads 0x80.. as the `i_*_extra` fields. setxattr must not put an
    /// area at 0x80: it gives the inode the extra fields first, as the
    /// kernel does, and the attribute goes after them.
    #[test]
    fn setxattr_on_an_inode_with_no_extra_isize_does_not_write_at_0x80() {
        let dev = formatted();
        let fs = mount(&dev);
        let ino = fs.apply_create("/f", 0o644).expect("create");
        patch_inode(&fs, ino, |raw| raw[0x80..].fill(0));
        fs.apply_setxattr("/f", "user.a", b"v").expect("setxattr");
        let (inode, raw) = fs
            .read_inode_verified(ino)
            .expect("the inode still verifies");
        let extra = u16::from_le_bytes(raw[0x80..0x82].try_into().unwrap());
        assert_eq!(
            extra,
            32,
            "i_extra_isize: the xattr went where the kernel reads i_*_extra: {:02x?}",
            &raw[0x80..0xA4]
        );
        assert_eq!(
            raw[0xA0..0xA4],
            crate::xattr::EXT4_XATTR_MAGIC.to_le_bytes(),
            "the in-inode area starts after the extra fields"
        );
        assert_eq!(
            crate::xattr::get_resolved(&fs, &inode, &raw, "user.a").unwrap(),
            Some(b"v".to_vec())
        );
    }

    /// An area a writer put at 0x80 is not an area the kernel reads, and
    /// neither is it one this crate reads or removes.
    #[test]
    fn an_xattr_area_at_0x80_is_not_read() {
        let dev = formatted();
        let fs = mount(&dev);
        let ino = fs.apply_create("/f", 0o644).expect("create");
        patch_inode(&fs, ino, |raw| {
            raw[0x80..].fill(0);
            let mut region = raw[0x80..].to_vec();
            crate::xattr::plan_set_in_inode_region(&mut region, "user.a", b"v").unwrap();
            raw[0x80..].copy_from_slice(&region);
            // What makes it look like one: i_extra_isize reads 0.
            assert_eq!(raw[0x80..0x82], [0, 0]);
        });
        let (inode, raw) = fs.read_inode_verified(ino).unwrap();
        assert_eq!(
            crate::xattr::get_resolved(&fs, &inode, &raw, "user.a").unwrap(),
            None
        );
    }

    /// With `i_extra_isize < 4` there is no `i_checksum_hi`: 0x82..0x84 is
    /// ordinary inode bytes, covered by the checksum and compared by
    /// nobody. Writing the checksum must not overwrite them, and verifying
    /// must compare only the low 16 bits, as the kernel and libext2fs do.
    #[test]
    fn an_inode_without_room_for_checksum_hi_keeps_its_bytes_at_0x82() {
        let dev = formatted();
        let fs = mount(&dev);
        assert!(fs.csum.enabled, "fixture: a metadata_csum volume");
        let ino = fs.apply_create("/f", 0o644).expect("create");
        let (inode, mut raw) = fs.read_inode_verified(ino).unwrap();
        raw[0x80..].fill(0);
        raw[0x82..0x84].copy_from_slice(&[0x02, 0xEA]);
        // The kernel's checksum of this inode: 16 bits, over every byte
        // but i_checksum_lo, 0x82..0x84 included.
        let (lo, _) = fs
            .csum
            .compute_inode_checksum(ino, inode.generation, &raw)
            .unwrap();
        raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
        assert!(
            fs.csum.verify_inode(ino, inode.generation, &raw),
            "a kernel-checksummed inode with i_extra_isize = 0 verifies"
        );
        let mut rewritten = raw.clone();
        fs.finalize_inode_raw(ino, inode.generation, &mut rewritten)
            .unwrap();
        assert_eq!(rewritten, raw, "re-checksumming changes nothing");
    }

    /// Overwrite an inode's raw record in place, re-stamping its checksum.
    fn audit70_patch_inode(fs: &Filesystem, ino: u32, f: impl FnOnce(&mut Vec<u8>)) {
        let (inode, mut raw) = fs.read_inode_verified(ino).unwrap();
        f(&mut raw);
        fs.finalize_inode_raw(ino, inode.generation, &mut raw)
            .unwrap();
        fs.write_inode_raw(ino, &raw).unwrap();
    }

    fn audit70_set_links(fs: &Filesystem, ino: u32, count: u16) {
        audit70_patch_inode(fs, ino, |raw| {
            raw[0x1A..0x1C].copy_from_slice(&count.to_le_bytes())
        });
    }

    fn audit70_links(fs: &Filesystem, ino: u32) -> u16 {
        fs.read_inode_verified(ino).unwrap().0.links_count
    }

    /// Set a RO_COMPAT bit on a formatted volume, fixing the superblock
    /// checksum so the result still mounts.
    fn set_ro_compat_bit(dev: &std::sync::Arc<MemDev>, bit: u32) {
        let mut sb = vec![0u8; 1024];
        dev.read_at(crate::superblock::SUPERBLOCK_OFFSET, &mut sb)
            .expect("read sb");
        let cur = u32::from_le_bytes(sb[0x64..0x68].try_into().unwrap());
        sb[0x64..0x68].copy_from_slice(&(cur | bit).to_le_bytes());
        let csum = crate::checksum::linux_crc32c(!0, &sb[..0x3FC]);
        sb[0x3FC..0x400].copy_from_slice(&csum.to_le_bytes());
        dev.write_at(crate::superblock::SUPERBLOCK_OFFSET, &sb)
            .expect("write sb");
    }

    /// #385: a directory link count of 1 means "too many to count"; rmdir
    /// of a child must leave it at 1, not write the 0 that makes Linux
    /// refuse the directory.
    #[test]
    fn audit70_rmdir_under_a_dir_nlink_parent_keeps_its_count() {
        let dev = formatted();
        set_ro_compat_bit(&dev, crate::features::RoCompat::DIR_NLINK.bits());
        let fs = mount(&dev);
        fs.apply_mkdir("/p", 0o755).unwrap();
        fs.apply_mkdir("/p/c", 0o755).unwrap();
        let p = resolve(&fs, "/p").unwrap();
        audit70_set_links(&fs, p, 1);
        fs.apply_rmdir("/p/c").unwrap();
        assert_eq!(
            audit70_links(&fs, p),
            1,
            "a directory's count of 1 is not decremented"
        );
    }

    /// #385: mkdir under a parent whose count is already "too many to
    /// count" leaves it there rather than writing a literal 2.
    #[test]
    fn audit70_mkdir_under_a_dir_nlink_parent_keeps_its_count() {
        let dev = formatted();
        set_ro_compat_bit(&dev, crate::features::RoCompat::DIR_NLINK.bits());
        let fs = mount(&dev);
        fs.apply_mkdir("/p", 0o755).unwrap();
        let p = resolve(&fs, "/p").unwrap();
        audit70_set_links(&fs, p, 1);
        fs.apply_mkdir("/p/c", 0o755).unwrap();
        assert_eq!(audit70_links(&fs, p), 1);
    }

    /// #385: the subdirectory that takes a DIR_NLINK parent past
    /// EXT4_LINK_MAX pins the count at 1, as the kernel does.
    #[test]
    fn audit70_mkdir_past_the_maximum_pins_a_dir_nlink_parent_at_one() {
        let dev = formatted();
        set_ro_compat_bit(&dev, crate::features::RoCompat::DIR_NLINK.bits());
        let fs = mount(&dev);
        fs.apply_mkdir("/p", 0o755).unwrap();
        let p = resolve(&fs, "/p").unwrap();
        audit70_set_links(&fs, p, 65000);
        fs.apply_mkdir("/p/c", 0o755).unwrap();
        assert_eq!(audit70_links(&fs, p), 1);
    }

    /// #385: without DIR_NLINK a directory at EXT4_LINK_MAX takes no more
    /// subdirectories: EMLINK, and nothing is written.
    #[test]
    fn audit70_mkdir_past_the_maximum_without_dir_nlink_is_refused() {
        let dev = formatted();
        let fs = mount(&dev);
        fs.apply_mkdir("/p", 0o755).unwrap();
        let p = resolve(&fs, "/p").unwrap();
        audit70_set_links(&fs, p, 65000);
        let r = fs.apply_mkdir("/p/c", 0o755);
        assert!(
            matches!(r, Err(Error::TooManyLinks)),
            "EMLINK expected, got {r:?}; count now {}",
            audit70_links(&fs, p)
        );
        assert_eq!(audit70_links(&fs, p), 65000);
        assert!(
            resolve(&fs, "/p/c").is_err(),
            "the refused directory was not created"
        );
    }

    /// #385: a hard link past the link maximum is refused, not wrapped
    /// to zero.
    #[test]
    fn audit70_a_link_past_the_maximum_is_refused_not_wrapped() {
        let dev = formatted();
        let fs = mount(&dev);
        let f = fs.apply_create("/f", 0o644).unwrap();
        audit70_set_links(&fs, f, 65535);
        let r = fs.apply_link("/f", "/g");
        assert!(
            matches!(r, Err(Error::TooManyLinks)),
            "EMLINK expected; link count now {}",
            audit70_links(&fs, f)
        );
        assert_eq!(audit70_links(&fs, f), 65535);
    }

    /// #385: EXT4_LINK_MAX (65000) is the ceiling, as in the kernel's
    /// `ext4_link`, not the width of the field.
    #[test]
    fn audit70_a_link_at_ext4_link_max_is_refused() {
        let dev = formatted();
        let fs = mount(&dev);
        let f = fs.apply_create("/f", 0o644).unwrap();
        audit70_set_links(&fs, f, 65000);
        assert!(matches!(
            fs.apply_link("/f", "/g"),
            Err(Error::TooManyLinks)
        ));
        assert!(resolve(&fs, "/g").is_err());
        audit70_set_links(&fs, f, 64999);
        fs.apply_link("/f", "/g").unwrap();
        assert_eq!(audit70_links(&fs, f), 65000);
    }

    /// #385: renaming a directory into a parent at EXT4_LINK_MAX on a
    /// volume without DIR_NLINK is refused, like mkdir there.
    #[test]
    fn audit70_rename_of_a_dir_into_a_full_parent_is_refused() {
        let dev = formatted();
        let fs = mount(&dev);
        fs.apply_mkdir("/p", 0o755).unwrap();
        fs.apply_mkdir("/d", 0o755).unwrap();
        let p = resolve(&fs, "/p").unwrap();
        audit70_set_links(&fs, p, 65000);
        assert!(matches!(
            fs.apply_rename("/d", "/p/d", false),
            Err(Error::TooManyLinks)
        ));
        assert_eq!(audit70_links(&fs, p), 65000);
        assert!(
            resolve(&fs, "/d").is_ok(),
            "the refused rename left the source alone"
        );
    }

    /// #385: the kernel's `ext4_inc_count` / `ext4_dec_count`, step by step.
    #[test]
    fn next_links_count_follows_the_kernel() {
        // Files: up to EXT4_LINK_MAX and no further; down to 0 and no further.
        assert_eq!(next_links_count(false, 64999, 1, true).unwrap(), 65000);
        assert!(matches!(
            next_links_count(false, 65000, 1, true),
            Err(Error::TooManyLinks)
        ));
        assert_eq!(next_links_count(false, 1, -1, true).unwrap(), 0);
        assert_eq!(next_links_count(false, 0, -1, true).unwrap(), 0);
        // Directories: pinned at 1 past the maximum with DIR_NLINK, refused without.
        assert_eq!(next_links_count(true, 65000, 1, true).unwrap(), 1);
        assert!(matches!(
            next_links_count(true, 65000, 1, false),
            Err(Error::TooManyLinks)
        ));
        assert_eq!(next_links_count(true, 1, 1, false).unwrap(), 1);
        // A directory count never drops through 2 or out of the pinned 1.
        assert_eq!(next_links_count(true, 3, -1, true).unwrap(), 2);
        assert_eq!(next_links_count(true, 2, -1, true).unwrap(), 2);
        assert_eq!(next_links_count(true, 1, -1, true).unwrap(), 1);
        // A summed delta is applied one link at a time.
        assert_eq!(next_links_count(true, 64999, 2, true).unwrap(), 1);
        assert_eq!(next_links_count(true, 5, -2, true).unwrap(), 3);
    }

    /// #385: fsck accepts a directory count of 1 on a DIR_NLINK volume --
    /// it is the "too many to count" value, not a count that is too low.
    #[test]
    fn audit70_fsck_accepts_a_dir_nlink_count_of_one() {
        let dev = formatted();
        set_ro_compat_bit(&dev, crate::features::RoCompat::DIR_NLINK.bits());
        let fs = mount(&dev);
        fs.apply_mkdir("/p", 0o755).unwrap();
        fs.apply_mkdir("/p/c", 0o755).unwrap();
        let p = resolve(&fs, "/p").unwrap();
        audit70_set_links(&fs, p, 1);
        let report = fs.audit(u32::MAX, u32::MAX).unwrap();
        assert!(report.is_clean(), "{:?}", report.anomalies);
    }

    /// The type byte of the root-directory entry `name`.
    fn root_entry_type(fs: &Filesystem, name: &[u8]) -> crate::dir::DirEntryType {
        let (root, _) = fs.read_inode_verified(2).unwrap();
        let phys = fs.map_inode_logical(&root, 0).unwrap().unwrap();
        let blk = fs.read_block(phys).unwrap();
        crate::dir::DirBlockIter::new(&blk, true)
            .map(|e| e.unwrap())
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{} not found", String::from_utf8_lossy(name)))
            .file_type
    }

    /// #386: rename keeps the entry's file type for every special file --
    /// a FIFO, a socket and both device kinds -- in both of its paths.
    #[test]
    fn audit70_rename_keeps_a_special_files_type() {
        use crate::dir::DirEntryType;
        let dev = formatted();
        let fs = mount(&dev);
        for (name, mode, want) in [
            ("fifo", 0o010644u16, DirEntryType::Fifo),
            ("sock", 0o140644, DirEntryType::Socket),
            ("chr", 0o020644, DirEntryType::CharDev),
            ("blk", 0o060644, DirEntryType::BlockDev),
        ] {
            let src = format!("/{name}");
            fs.apply_mknod(&src, mode, 1, 3).unwrap();
            // The no-overwrite path.
            let moved = format!("/{name}_moved");
            fs.apply_rename(&src, &moved, false).unwrap();
            assert_eq!(
                root_entry_type(&fs, &moved.as_bytes()[1..]),
                want,
                "{name}: rename"
            );
            // The replace path.
            let victim = format!("/{name}_victim");
            fs.apply_create(&victim, 0o644).unwrap();
            fs.apply_rename(&moved, &victim, true).unwrap();
            assert_eq!(
                root_entry_type(&fs, &victim.as_bytes()[1..]),
                want,
                "{name}: rename over a file"
            );
        }
    }

    /// #427: an inline-data directory laid out the way the kernel lays one
    /// out -- `i_block` holds the parent's inode number, then entries from
    /// byte 4; entries that do not fit continue in the `system.data` xattr
    /// -- is read by both the lookup and the readdir path.
    mod inline_dir_reads {
        use super::*;
        use crate::inode::{InodeFlags, OFF_BLOCK, OFF_FLAGS, OFF_SIZE_LO};

        /// One directory entry, `rec_len` bytes long.
        fn dirent(ino: u32, name: &[u8], file_type: u8, rec_len: u16) -> Vec<u8> {
            let mut e = vec![0u8; rec_len as usize];
            e[0..4].copy_from_slice(&ino.to_le_bytes());
            e[4..6].copy_from_slice(&rec_len.to_le_bytes());
            e[6] = name.len() as u8;
            e[7] = file_type;
            e[8..8 + name.len()].copy_from_slice(name);
            e
        }

        /// Rewrite the directory `dir` in the kernel's inline layout:
        /// `i_block` = `parent` + `in_block` (56 bytes), and `continuation`
        /// as the `system.data` xattr value; `i_size` = 60 + its length.
        fn make_inline_dir(
            fs: &Filesystem,
            dir: u32,
            parent: u32,
            in_block: &[u8],
            continuation: &[u8],
        ) {
            assert_eq!(in_block.len(), 56, "i_block holds 56 bytes of entries");
            let (inode, mut raw) = fs.read_inode_verified(dir).unwrap();
            let flags =
                (inode.flags & !InodeFlags::EXTENTS.bits()) | InodeFlags::INLINE_DATA.bits();
            raw[OFF_FLAGS..OFF_FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
            raw[OFF_BLOCK..OFF_BLOCK + 4].copy_from_slice(&parent.to_le_bytes());
            raw[OFF_BLOCK + 4..OFF_BLOCK + 60].copy_from_slice(in_block);
            let size = 60 + continuation.len() as u32;
            raw[OFF_SIZE_LO..OFF_SIZE_LO + 4].copy_from_slice(&size.to_le_bytes());
            let extra = u16::from_le_bytes(raw[0x80..0x82].try_into().unwrap()) as usize;
            let end = (fs.sb.inode_size as usize).min(raw.len());
            crate::xattr::plan_set_in_inode_region(
                &mut raw[128 + extra..end],
                "system.data",
                continuation,
            )
            .expect("system.data fits in the inode");
            fs.finalize_inode_raw(dir, inode.generation, &mut raw)
                .unwrap();
            fs.write_inode_raw(dir, &raw).unwrap();
        }

        /// A volume with `/d` inline: `x` and `sub` in `i_block`, `zz` in
        /// the continuation. Returns the device and the inode numbers.
        fn volume() -> (std::sync::Arc<MemDev>, u32, u32, u32, u32) {
            let dev = formatted();
            set_incompat_bit(&dev, crate::features::Incompat::INLINE_DATA.bits());
            let fs = mount(&dev);
            let d = fs.apply_mkdir("/d", 0o755).unwrap();
            let x = fs.apply_create("/x", 0o644).unwrap();
            let sub = fs.apply_mkdir("/sub", 0o755).unwrap();
            let zz = fs.apply_create("/zz", 0o644).unwrap();
            let mut in_block = dirent(x, b"x", 1, 12);
            in_block.extend(dirent(sub, b"sub", 2, 44));
            make_inline_dir(&fs, d, 2, &in_block, &dirent(zz, b"zz", 1, 20));
            drop(fs);
            (dev, d, x, sub, zz)
        }

        #[test]
        fn a_lookup_reads_entries_from_byte_4_and_from_the_continuation() {
            let (dev, d, x, sub, zz) = volume();
            let fs = mount(&dev);
            assert!(fs.read_inode_verified(d).unwrap().0.has_inline_data());
            for (path, want) in [
                ("/d/x", x),
                ("/d/sub", sub),
                ("/d/zz", zz),
                ("/d/.", d),
                ("/d/..", 2),
            ] {
                assert_eq!(
                    resolve(&fs, path).map_err(|e| format!("{e:?}")),
                    Ok(want),
                    "{path}"
                );
            }
            assert!(matches!(resolve(&fs, "/d/nope"), Err(Error::NotFound)));
        }

        #[test]
        fn readdir_lists_the_synthesised_dots_the_block_entries_and_the_continuation() {
            use std::ffi::{CStr, CString};
            let (dev, d, x, sub, zz) = volume();
            let image = fs_ext4_test_support::temp_dir()
                .join(format!("fs_ext4_inline_readdir_{}.img", std::process::id()));
            std::fs::write(&image, &*dev.bytes.lock().unwrap()).unwrap();
            let path = CString::new(image.to_str().unwrap()).unwrap();
            let mut listed = Vec::new();
            let err = unsafe {
                let fs = crate::capi::fs_ext4_mount(path.as_ptr());
                assert!(!fs.is_null(), "mount");
                let dir = CString::new("/d").unwrap();
                let it = crate::capi::fs_ext4_dir_open(fs, dir.as_ptr());
                let err = if it.is_null() {
                    Some(
                        CStr::from_ptr(crate::capi::fs_ext4_last_error())
                            .to_string_lossy()
                            .into_owned(),
                    )
                } else {
                    loop {
                        let e = crate::capi::fs_ext4_dir_next(it);
                        if e.is_null() {
                            break;
                        }
                        let name = CStr::from_ptr((*e).name.as_ptr())
                            .to_string_lossy()
                            .into_owned();
                        listed.push((name, (*e).inode));
                    }
                    crate::capi::fs_ext4_dir_close(it);
                    None
                };
                crate::capi::fs_ext4_umount(fs);
                err
            };
            let _ = std::fs::remove_file(&image);
            assert_eq!(err, None, "dir_open /d");
            listed.sort();
            let mut want: Vec<(String, u32)> =
                [(".", d), ("..", 2), ("sub", sub), ("x", x), ("zz", zz)]
                    .iter()
                    .map(|(n, i)| (n.to_string(), *i))
                    .collect();
            want.sort();
            assert_eq!(listed, want);
        }

        #[test]
        fn the_inode_api_reads_an_inline_directory() {
            let (dev, d, x, sub, zz) = volume();
            let fs = mount(&dev);
            for (name, want) in [
                (&b"x"[..], x),
                (b"sub", sub),
                (b"zz", zz),
                (b".", d),
                (b"..", 2),
            ] {
                assert_eq!(
                    fs.lookup_at(d, name).map_err(|e| format!("{e:?}")),
                    Ok(want),
                    "lookup_at {}",
                    String::from_utf8_lossy(name)
                );
            }
            assert!(matches!(fs.lookup_at(d, b"nope"), Err(Error::NotFound)));
            let mut listed: Vec<(Vec<u8>, u32)> = fs
                .read_dir_ino(d)
                .map_err(|e| format!("{e:?}"))
                .expect("read_dir_ino")
                .into_iter()
                .map(|e| (e.name, e.inode))
                .collect();
            listed.sort();
            let mut want: Vec<(Vec<u8>, u32)> =
                [(".", d), ("..", 2), ("sub", sub), ("x", x), ("zz", zz)]
                    .iter()
                    .map(|(n, i)| (n.as_bytes().to_vec(), *i))
                    .collect();
            want.sort();
            assert_eq!(listed, want);
        }
    }
}
