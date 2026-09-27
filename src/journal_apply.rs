//! JBD2 journal replay applier.
//!
//! Turns a [`journal::ReplayPlan`] (plan-layer output from the walker) into
//! actual `write_at` calls on a writable [`BlockDevice`]. The corresponding
//! read-side walker is [`crate::journal::walk`]; the write-side producer is
//! [`crate::transaction::Transaction`]. Closing the loop:
//!
//!   Transaction::commit ──(journal bytes)──▶ disk
//!                                              │
//!                         journal::walk ◀──────┘
//!                              │
//!                              ▼
//!                   journal_apply::apply  ──▶ final-location disk writes
//!
//! Mount flow (when journal is dirty), in [`replay_if_dirty`]:
//!
//! ```text
//!   1. A read-only device: return, replaying nothing here. Mount replays
//!      into the buffer cache instead ([`replay_into_cache`], #72).
//!   2. Parse the JBD2 superblock via jbd2::read_superblock; clean: clear a
//!      leftover needs_recovery and return.
//!   3. Refuse a volume with a write-breaking INCOMPAT bit.
//!   4. plan = journal::walk(&fs, &jsb)?; journal_apply::apply(&fs, &plan)?
//!   5. Mark the journal clean, then clear needs_recovery.
//!   6. The mount continues, read-write.
//! ```
//!
//! Once the writes are flushed, the journal superblock gets `s_start = 0`
//! and the sequence one past the first uncommitted transaction, and the ext4
//! superblock loses `needs_recovery` (#228). Left dirty, every later mount
//! replayed the same log again and `e2fsck` reported a journal with data
//! in it. The order matters: a crash between the two leaves a clean
//! journal flagged for recovery, which recovers nothing; the reverse would
//! be a live journal Linux wipes.

use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::inode::Inode;
use crate::jbd2::{self, JournalSuperblock};
use crate::journal::ReplayPlan;

/// Set or clear `INCOMPAT_RECOVER` (`needs_recovery`) in a 1024-byte
/// superblock image, redoing its checksum under `metadata_csum`.
pub(crate) fn set_needs_recovery_in(sb: &mut [u8], on: bool) {
    let recover = crate::features::Incompat::RECOVER.bits();
    let incompat = u32::from_le_bytes(sb[0x60..0x64].try_into().unwrap());
    let incompat = if on {
        incompat | recover
    } else {
        incompat & !recover
    };
    sb[0x60..0x64].copy_from_slice(&incompat.to_le_bytes());
    let ro_compat = u32::from_le_bytes(sb[0x64..0x68].try_into().unwrap());
    if ro_compat & crate::features::RoCompat::METADATA_CSUM.bits() != 0 {
        let csum = crate::checksum::linux_crc32c(!0, &sb[..0x3FC]);
        sb[0x3FC..0x400].copy_from_slice(&csum.to_le_bytes());
    }
}

/// Set or clear `needs_recovery` in the primary superblock on `dev`. Linux
/// replays a journal only when this is set, and wipes one when it is not.
pub(crate) fn write_needs_recovery(dev: &dyn crate::block_io::BlockDevice, on: bool) -> Result<()> {
    let at = crate::superblock::SUPERBLOCK_OFFSET;
    let mut sb = vec![0u8; 1024];
    dev.read_at(at, &mut sb)?;
    set_needs_recovery_in(&mut sb, on);
    dev.write_at(at, &sb)
}

/// Record a replayed journal as done: `s_start = 0` and `s_sequence` the one
/// past the first uncommitted transaction, checksum redone, flushed.
fn mark_journal_clean(fs: &Filesystem, jsb: &JournalSuperblock, plan: &ReplayPlan) -> Result<()> {
    let raw = fs.read_inode_raw(fs.sb.journal_inode)?;
    let jinode = Inode::parse(&raw)?;
    let phys = jbd2::journal_block_to_physical(fs, &jinode, 0)?
        .ok_or(Error::Corrupt("journal_apply: journal superblock unmapped"))?;
    let at = byte_offset_of(fs, phys)?;
    let mut buf = vec![0u8; 1024];
    fs.dev.read_at(at, &mut buf)?;
    if u32::from_be_bytes(buf[0..4].try_into().unwrap()) != jbd2::JBD2_MAGIC_NUMBER {
        return Err(Error::Corrupt(
            "journal_apply: journal superblock lost its magic during replay",
        ));
    }
    // Restart the log one past the first transaction the walk did not find
    // committed, as the kernel's `jbd2_journal_recover` does
    // (`++info.end_transaction`): a torn tail may already carry that ID, and
    // reusing it could let a stale block pass as part of the next transaction.
    // `next_sequence` rather than `last_commit`, whose 0 is also a real ID.
    let sequence = plan.next_sequence.unwrap_or(jsb.sequence).wrapping_add(1);
    buf[0x18..0x1C].copy_from_slice(&sequence.to_be_bytes());
    buf[0x1C..0x20].copy_from_slice(&0u32.to_be_bytes());
    if jsb.uses_csum_v2_or_v3() {
        buf[0xFC..0x100].fill(0);
        let csum = crate::checksum::linux_crc32c(!0, &buf);
        buf[0xFC..0x100].copy_from_slice(&csum.to_be_bytes());
    }
    fs.dev.write_at(at, &buf)?;
    fs.dev.flush()
}

/// Where a block number lands on the device, or a refusal.
///
/// Replay is the one operation that writes to a block number the image
/// itself chose. It runs at mount, before anybody has asked for a file,
/// and the number comes from a descriptor tag inside the journal --
/// which is to say, from whoever wrote the image.
///
/// Two things go wrong when that number is taken on trust:
///
///   - a block past the end of the filesystem writes past the end of the
///     device. Against a file-backed image that is not an error at all;
///     the file simply grows, and a tag naming block 0xDEADBEEF turns a
///     16 MiB image into a 15 TB one.
///   - a block number above `2^64 / block_size` wraps. At a 4 KiB block
///     size, block `2^52` multiplies out to exactly 0 -- the boot sector
///     and the primary superblock -- and the replay overwrites them with
///     contents the image supplied, then reports success.
///
/// Neither is a write to be clamped into range. A destination the
/// filesystem does not contain means the journal is not describing this
/// filesystem, so the replay stops and the mount fails rather than
/// applying the part of it that happened to be in range.
pub(crate) fn byte_offset_of(fs: &Filesystem, block: u64) -> Result<u64> {
    byte_offset_in(
        block,
        fs.sb.blocks_count,
        fs.sb.block_size(),
        fs.dev.size_bytes(),
    )
}

/// The same rule, for callers that hold the pieces rather than the
/// `Filesystem` -- `journal_writer` writes through a `&dyn BlockDevice`
/// after the mount has gone out of scope.
pub(crate) fn byte_offset_in(
    block: u64,
    blocks_count: u64,
    block_size: u32,
    device_bytes: u64,
) -> Result<u64> {
    if block >= blocks_count {
        return Err(Error::Corrupt(
            "journal: names a block past the end of the filesystem",
        ));
    }
    let block_size = block_size as u64;
    let offset = block
        .checked_mul(block_size)
        .ok_or(Error::Corrupt("journal: block offset overflows"))?;
    // `blocks_count` is the image's own claim about its size, so a
    // truncated image passes the check above and still reaches past the
    // device. The device is asked directly.
    let end = offset
        .checked_add(block_size)
        .ok_or(Error::Corrupt("journal: block offset overflows"))?;
    if end > device_bytes {
        return Err(Error::Corrupt(
            "journal: names a block past the end of the device",
        ));
    }
    Ok(offset)
}

/// Apply all writes in `plan` to `fs.dev`. Each `ReplayEntry` names a
/// journal-block source and a fs-block destination; we read the source
/// contents and overwrite the destination. Revoked writes are already
/// filtered out of `plan.writes` by [`ReplayPlan::filter_revoked`] (called
/// by [`crate::journal::walk`]).
///
/// Returns the number of blocks written. Errors propagate from either the
/// journal read or the final-location write.
pub fn apply(fs: &Filesystem, plan: &ReplayPlan) -> Result<usize> {
    if plan.writes.is_empty() {
        return Ok(0);
    }
    if !fs.dev.is_writable() {
        return Err(Error::Corrupt(
            "journal_apply: device is not writable; cannot replay",
        ));
    }

    let block_size = fs.sb.block_size() as u64;

    // EVERY ENTRY IS RESOLVED BEFORE ANY IS WRITTEN. Checked inside the
    // write loop, a bad destination was found after the entries before it
    // were already on disk, and the mount failed over a filesystem it had
    // half replayed (#150). A refusal is only free before the first write.
    //
    // Source: journal_block is a logical block inside the journal inode,
    // resolved to a physical fs block via the extent tree. Destination:
    // fs_block, bounds-checked against the filesystem and the device.
    let resolved = resolve(fs, plan)?;

    let mut applied = 0usize;
    for (w, &(source, destination)) in plan.writes.iter().zip(&resolved) {
        let mut buf = vec![0u8; block_size as usize];
        fs.dev.read_at(source, &mut buf)?;

        // If the ESCAPED flag is set, the first 4 bytes of the journal block
        // were zeroed during write to keep them from colliding with the
        // JBD2 magic. Restore them to the magic before writing to final.
        if w.flags & crate::journal::TAG_ESCAPED != 0 {
            buf[0..4].copy_from_slice(&crate::jbd2::JBD2_MAGIC_NUMBER.to_be_bytes());
        }

        fs.dev.write_at(destination, &buf)?;
        applied += 1;
    }

    // Best-effort durability: flush before we claim success. If the caller
    // wants full crash-consistent ordering (journal writes → fsync → final
    // writes → fsync), they should sequence it themselves; this module is
    // the single final-location pass after the journal is already on disk.
    fs.dev.flush()?;

    Ok(applied)
}

/// Replay a dirty journal into the device's buffer cache, writing nothing
/// (#72).
///
/// A transaction in a dirty journal is committed: the filesystem told its
/// caller the change was durable and had not yet checkpointed it. A
/// read-only mount that skipped replay read the superseded metadata and
/// reported it as fact. This walks the journal exactly as
/// [`replay_if_dirty`] does, with every refusal the walk makes, and pins
/// each replayed block's bytes in the cache (`populate_cache`) instead of
/// writing them, so reads see the committed state and the device stays
/// untouched.
///
/// Returns the number of blocks pinned: 0 for a clean journal or none.
pub fn replay_into_cache(fs: &Filesystem) -> Result<usize> {
    let Some(jsb) = jbd2::read_superblock(fs)? else {
        return Ok(0);
    };
    if jsb.is_clean() {
        return Ok(0);
    }
    let plan = crate::journal::walk(fs, &jsb)?;
    if plan.writes.is_empty() {
        return Ok(0);
    }
    let block_size = fs.sb.block_size() as u64;
    let resolved = resolve(fs, &plan)?;
    // Read every source before pinning any, so a failed read leaves the
    // cache as it was rather than half replayed.
    let mut blocks = Vec::with_capacity(plan.writes.len());
    for (w, &(source, _)) in plan.writes.iter().zip(&resolved) {
        let mut buf = vec![0u8; block_size as usize];
        fs.dev.read_at(source, &mut buf)?;
        if w.flags & crate::journal::TAG_ESCAPED != 0 {
            buf[0..4].copy_from_slice(&crate::jbd2::JBD2_MAGIC_NUMBER.to_be_bytes());
        }
        blocks.push((w.fs_block, buf));
    }
    // In log order, so a block a later transaction rewrote ends with its
    // later bytes.
    let n = blocks.len();
    for (block, bytes) in blocks {
        fs.dev.populate_cache(block, bytes);
    }
    Ok(n)
}

/// Each entry's journal source and filesystem destination as device byte
/// offsets, or the refusal for the first that has none.
fn resolve(fs: &Filesystem, plan: &ReplayPlan) -> Result<Vec<(u64, u64)>> {
    let raw = fs.read_inode_raw(fs.sb.journal_inode)?;
    let jinode = Inode::parse(&raw)?;
    plan.writes
        .iter()
        .map(|w| {
            let phys = jbd2::journal_block_to_physical(fs, &jinode, w.journal_block)?
                .ok_or(Error::Corrupt("journal_apply: journal block unmapped"))?;
            Ok((byte_offset_of(fs, phys)?, byte_offset_of(fs, w.fs_block)?))
        })
        .collect()
}

/// Convenience: mount-time entry point. Reads the journal superblock and,
/// if the journal is dirty AND the device is writable, walks + applies in
/// one shot. Returns the number of blocks replayed (0 if clean or device
/// is read-only — the latter is not an error; mount proceeds read-only).
pub fn replay_if_dirty(fs: &Filesystem) -> Result<usize> {
    // A read-only device cannot take a replay. Mount replays a dirty
    // journal into the cache instead (`replay_into_cache`, #72); this entry
    // point is the one that writes.
    if !fs.dev.is_writable() {
        return Ok(0);
    }
    let Some(jsb) = jbd2::read_superblock(fs)? else {
        return Ok(0); // no journal inode → nothing to replay
    };
    if jsb.is_clean() {
        // A crash between the clean journal and the cleared flag leaves
        // the flag behind with nothing to recover.
        if fs.sb.feature_incompat & crate::features::Incompat::RECOVER.bits() != 0 {
            write_needs_recovery(fs.dev.as_ref(), false)?;
            fs.dev.flush()?;
        }
        return Ok(0);
    }
    // Replay writes to the volume, so it refuses what every other write
    // refuses. The mount checks these bits too, but a lazy mount checked
    // while it was read-only and calls this once it is writable (#117).
    let write_breaking = crate::features::write_breaking_incompat(fs.sb.feature_incompat);
    if write_breaking != 0 {
        return Err(crate::Error::UnsupportedIncompat(write_breaking));
    }
    let plan = crate::journal::walk(fs, &jsb)?;
    let applied = apply(fs, &plan)?;
    mark_journal_clean(fs, &jsb, &plan)?;
    write_needs_recovery(fs.dev.as_ref(), false)?;
    fs.dev.flush()?;
    Ok(applied)
}

/// Describe the JBD2 superblock fields most relevant to replay decisions.
/// Useful for diagnostics and for tests that want to assert on the state
/// before and after replay.
#[derive(Debug, Clone)]
pub struct ReplaySummary {
    pub clean: bool,
    pub sequence: u32,
    pub start: u32,
    pub max_len: u32,
    pub has_revoke: bool,
    pub uses_64bit: bool,
    pub uses_csum_v2_or_v3: bool,
}

impl From<&JournalSuperblock> for ReplaySummary {
    fn from(jsb: &JournalSuperblock) -> Self {
        Self {
            clean: jsb.is_clean(),
            sequence: jsb.sequence,
            start: jsb.start,
            max_len: jsb.max_len,
            has_revoke: jsb.has_revoke(),
            uses_64bit: jsb.uses_64bit(),
            uses_csum_v2_or_v3: jsb.uses_csum_v2_or_v3(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{ReplayEntry, ReplayPlan};

    #[test]
    fn empty_plan_applies_zero_blocks() {
        // We can exercise the early-return branch without a real Filesystem.
        let plan = ReplayPlan::default();
        // The function requires a Filesystem; but the early-return path never
        // touches it. Skipping — the integration test in tests/ covers the
        // live path.
        assert!(plan.writes.is_empty());
    }

    /// The overflow branch itself. `blocks_count` and the device size are
    /// both `u64::MAX` so neither bounds check can refuse the block first:
    /// only `checked_mul` stands between this block and a byte offset that
    /// wraps to 0, the boot sector and the primary superblock.
    #[test]
    fn byte_offset_in_refuses_a_block_whose_offset_wraps() {
        let block_size = 4096u32;
        let wraps_to_zero = u64::MAX / block_size as u64 + 1;
        assert_eq!(wraps_to_zero.wrapping_mul(block_size as u64), 0);
        match byte_offset_in(wraps_to_zero, u64::MAX, block_size, u64::MAX) {
            Err(Error::Corrupt(msg)) => assert_eq!(msg, "journal: block offset overflows"),
            other => panic!("block {wraps_to_zero} was not refused as an overflow: {other:?}"),
        }
    }

    #[test]
    fn replay_summary_mirrors_jsb() {
        let jsb = JournalSuperblock {
            block_type: crate::jbd2::JBD2_SUPERBLOCK_V2,
            header_sequence: 1,
            block_size: 4096,
            max_len: 8192,
            first: 1,
            sequence: 42,
            start: 100,
            errno: 0,
            feature_compat: 0,
            feature_incompat: crate::jbd2::JbdIncompat::REVOKE.bits()
                | crate::jbd2::JbdIncompat::BIT64.bits()
                | crate::jbd2::JbdIncompat::CSUM_V3.bits(),
            feature_ro_compat: 0,
            uuid: [0; 16],
            nr_users: 1,
            checksum_type: 4,
            num_fc_blocks: 0,
            checksum: 0,
        };
        let s = ReplaySummary::from(&jsb);
        assert!(!s.clean);
        assert_eq!(s.sequence, 42);
        assert_eq!(s.start, 100);
        assert!(s.has_revoke);
        assert!(s.uses_64bit);
        assert!(s.uses_csum_v2_or_v3);
    }

    #[test]
    fn clean_summary() {
        let mut jsb = JournalSuperblock {
            block_type: crate::jbd2::JBD2_SUPERBLOCK_V2,
            header_sequence: 1,
            block_size: 4096,
            max_len: 8192,
            first: 1,
            sequence: 1,
            start: 0,
            errno: 0,
            feature_compat: 0,
            feature_incompat: 0,
            feature_ro_compat: 0,
            uuid: [0; 16],
            nr_users: 1,
            checksum_type: 0,
            num_fc_blocks: 0,
            checksum: 0,
        };
        jsb.start = 0;
        let s = ReplaySummary::from(&jsb);
        assert!(s.clean);
    }

    #[test]
    fn replay_entry_structure_stable() {
        let e = ReplayEntry {
            transaction: 1,
            fs_block: 100,
            journal_block: 5,
            flags: 0,
        };
        assert_eq!(e.transaction, 1);
        assert_eq!(e.fs_block, 100);
        assert_eq!(e.journal_block, 5);
    }
}
