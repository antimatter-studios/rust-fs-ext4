//! JBD2 live-write side: serialize a transaction, write it to the journal
//! inode, then apply the writes to their final fs locations and mark the
//! journal clean.
//!
//! The replay-side dual is [`crate::journal_apply`] (which the mount path
//! calls when the journal is dirty). The serialization dual is
//! [`crate::transaction::Transaction`] (which builds the journal-format
//! bytes). This module is the glue: it owns the journal inode's logical→
//! physical map, the JBD2 superblock cursor (`start` + `sequence`), and
//! the four-fence sequencing that makes a write crash-safe.
//!
//! ## Crash-safety contract — "journal then immediate checkpoint"
//!
//! Each [`JournalWriter::commit`] call performs five fenced steps:
//!
//! ```text
//!   1. Write transaction blocks (descriptor + data + commit) to journal
//!      logical blocks [1, 1+N), set `needs_recovery` in the ext4
//!      superblock → flush.
//!   2. Set jsb.start = 1 (mark journal dirty), keep jsb.sequence as-is →
//!      flush.
//!   3. Write each fs block in the transaction to its final on-disk
//!      location → flush.
//!   4. Set jsb.start = 0 (clean), bump jsb.sequence, clear
//!      `needs_recovery` → flush.
//! ```
//!
//! `needs_recovery` (`INCOMPAT_RECOVER`) is what Linux replays on: a
//! filesystem without it has its journal wiped at mount, whatever `s_start`
//! says (#228). So it reaches the disk before the journal is marked dirty,
//! and a transaction that journals the superblock block carries it set,
//! or step 3 would clear it while the journal is still live. Clearing it
//! in the same flush as the clean journal superblock is safe either way
//! round: by then step 3 is on disk, so a wiped journal loses nothing.
//!
//! Crash analysis:
//!
//! - Crash before step 2: jsb.start unchanged → walker yields empty plan
//!   → the partially-written journal is ignored.
//! - Crash between 2 and 3: jsb.start = 1 → walker reads one transaction,
//!   replay applies the writes (idempotent with what step 3 would have
//!   written).
//! - Crash between 3 and 4: same — replay re-applies; final-location
//!   bytes are already what they would be after replay, so it's a no-op.
//! - Crash after 4: clean state, walker yields empty plan.
//!
//! ## Limitations (deferred to later Phase 5 sub-items)
//!
//! - Transactions are bounded by `max_len - 2` blocks (one for sb, one for
//!   commit). Large file writes that exceed the journal capacity must be
//!   split by the caller; this module errors on overflow.
//! - No batching across calls. Every commit checkpoints immediately, so
//!   the journal only ever holds one transaction at rest. This is
//!   correctness-first; ring-style batching is a Phase 8 perf concern.
//! - The JBD2 superblock checksum (`s_checksum` at 0xFC) is recomputed by
//!   `write_jsb` whenever the journal is checksummed (CSUM_V2/V3), because we
//!   mutate `s_start` + `s_sequence` on every commit. A real Linux kernel AND
//!   `e2fsck` REFUSE a checksummed journal whose superblock checksum is stale
//!   ("Journal superblock is corrupt"), so leaving it unrecomputed bricks the
//!   fs on the next mount. v1 journals have no such field and are untouched.

use crate::block_io::BlockDevice;
use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::inode::Inode;
use crate::jbd2::{self, JournalSuperblock, JBD2_MAGIC_NUMBER};
use crate::transaction::Transaction;

/// Owns the live-write side of the JBD2 journal. Built once at mount time
/// (when the FS has a journal); reused across every mutating capi call.
///
/// Cheap to construct (one inode read + extent walk). Not thread-safe; the
/// outer Filesystem lock must serialize mutating ops anyway.
pub struct JournalWriter {
    healthy: bool,
    /// Leave `needs_recovery` set after a checkpoint rather than clearing
    /// it: a checked mount (`Filesystem::mount_recovering`) clears it only
    /// at `finish`, so the volume reads as in recovery while it is owned.
    hold_needs_recovery: bool,
    /// `physical_map[logical]` is the fs physical block backing journal
    /// logical block `logical`. Length = `jsb.max_len`. Block 0 is the
    /// JBD2 superblock; blocks 1.. carry transactions.
    physical_map: Vec<u64>,
    /// Block size of the underlying device (matches `jsb.block_size`).
    block_size: u32,
    /// Cached JBD2 superblock — mutated in memory + flushed to disk on
    /// every commit. The on-disk truth always matches this between calls.
    jsb: JournalSuperblock,
    /// How many blocks the filesystem has, so a write can be checked
    /// against it after the mount has gone out of scope.
    blocks_count: u64,
}

/// Whether a journal of `max_len` blocks fits both its inode and the device.
fn journal_len_fits(
    max_len: u32,
    inode_size: u64,
    block_size: u32,
    device_bytes: u64,
) -> Result<()> {
    let block_size = u64::from(block_size.max(1));
    if u64::from(max_len) > inode_size / block_size {
        return Err(Error::Corrupt(
            "journal declares more blocks than its inode holds",
        ));
    }
    if u64::from(max_len) > device_bytes / block_size {
        return Err(Error::Corrupt(
            "journal declares more blocks than the device holds",
        ));
    }
    Ok(())
}

impl JournalWriter {
    /// Open the writer for a mounted filesystem.
    ///
    /// Returns `Ok(None)` when the FS has no journal (`s_journal_inum == 0`)
    /// — callers should fall back to unjournaled writes in that case.
    /// Returns `Err` when the journal is misconfigured (e.g. the journal
    /// inode is missing or unparsable).
    ///
    /// Flavor-aware: walks the journal inode's block tree via
    /// [`crate::indirect::map_logical_any`], so ext3 journals (legacy
    /// indirect block pointers) work the same as ext4 ones (extent tree).
    pub fn open(fs: &Filesystem) -> Result<Option<Self>> {
        let Some(jsb) = jbd2::read_superblock(fs)? else {
            return Ok(None);
        };
        let raw = fs.read_inode_raw(fs.sb.journal_inode)?;
        let jinode = Inode::parse(&raw)?;

        // Build the full physical map up-front. For typical 32 MiB journals
        // at 4 KiB blocks that's 8192 entries — tiny. Allocates once at
        // mount; every commit then does a constant-time index.
        let bs = fs.sb.block_size();
        // A JOURNAL IS INSIDE THE FILESYSTEM IT JOURNALS.
        //
        // `s_maxlen` is a raw `u32` and nothing bounds it: 0xFFFFFFFF
        // reserved 34 GB here before a block was mapped, which
        // `handle_alloc_error` answers by aborting -- past
        // `ffi_guard`'s `catch_unwind`, killing the host extension.
        // The journal's blocks are filesystem blocks, so there cannot
        // be more of them than the filesystem has.
        if u64::from(jsb.max_len) > fs.sb.blocks_count {
            return Err(Error::Corrupt(
                "journal declares more blocks than the filesystem holds",
            ));
        }
        // `s_blocks_count` is a raw field too, so the bound above is only as
        // good as the image (#182). Two that are not the superblock's to
        // choose: the journal inode's own length, which is the check the
        // kernel makes ("journal file too short"), and the device, which a
        // journal on it cannot outgrow. `i_size` is raw as well; the device
        // bound is the one that caps the reservation below.
        journal_len_fits(jsb.max_len, jinode.size, bs, fs.dev.size_bytes())?;

        // NOR OVER THE BLOCKS EVERY MOUNT READS FIRST (#183). A commit
        // writes wherever this map says, and `journal_apply::byte_offset_in`
        // refuses only a block past the filesystem or the device -- block 0
        // passes, and a journal inode mapping a logical block there put the
        // first commit's descriptor over the primary superblock. So a
        // mapping onto block 0, the superblock, group 0's descriptor table
        // or its reserved growth, or past the end, is refused here, once,
        // rather than trusted at every write.
        let descriptor_bytes = fs.sb.block_group_count() * u64::from(fs.sb.desc_size);
        let group0_metadata_end = u64::from(fs.sb.first_data_block)
            + 1
            + descriptor_bytes.div_ceil(u64::from(bs))
            + u64::from(fs.sb.reserved_gdt_blocks);
        let mut physical_map = Vec::with_capacity(jsb.max_len as usize);
        for logical in 0..jsb.max_len as u64 {
            let phys = crate::indirect::map_logical_any(
                &jinode.block,
                jinode.flags,
                fs.dev.as_ref(),
                bs,
                logical,
            )?
            .ok_or(Error::Corrupt(
                "journal_writer: journal inode has unmapped logical block",
            ))?;
            if phys < group0_metadata_end || phys >= fs.sb.blocks_count {
                return Err(Error::Corrupt(
                    "journal_writer: journal inode maps a block onto the superblock, group 0's \
                     descriptor table, or past the end of the filesystem",
                ));
            }
            physical_map.push(phys);
        }

        Ok(Some(Self {
            healthy: true,
            hold_needs_recovery: false,
            blocks_count: fs.sb.blocks_count,
            physical_map,
            block_size: bs,
            jsb,
        }))
    }

    /// Begin a new transaction with the next sequence number. Caller adds
    /// writes, then calls [`Self::commit`] to publish.
    pub fn begin(&self) -> Transaction {
        Transaction::begin(
            self.jsb.sequence,
            self.block_size,
            self.jsb.uses_64bit(),
            // CSUM_V3 ALONE. `Transaction::begin`'s last parameter is
            // "does this journal use v3 tags", and it was handed "does
            // this journal use checksums at all" -- so on a CSUM_V2
            // journal the encoder laid 16-byte v3 tags while both this
            // crate's reader (`journal.rs`, keyed on CSUM_V3) and the
            // kernel parse classical ones. Tag 0 aliases
            // correctly, which hid it on single-block transactions;
            // from tag 1 on, the reader takes the writer's zeroed
            // checksum slot as `t_blocknr` -- fs block 0 -- so a
            // recovery replays a data block onto the ext4 superblock.
            // Every mkdir, create and unlink is multi-block.
            self.jsb.uses_csum_v3(),
        )
    }

    /// The capacity (in fs blocks) the caller can fit into one transaction.
    /// One block is reserved for the JBD2 superblock; the remaining
    /// `max_len - 1` carry [descriptor, data..., (revoke), commit].
    pub fn max_blocks_per_transaction(&self) -> usize {
        // -1 for sb. Caller's writes also need a descriptor + commit slot,
        // so payload-only capacity is roughly `max_len - 3` data blocks
        // when the descriptor's tag table fits in one block. We expose the
        // raw upper bound here; the commit path enforces the real limit.
        (self.jsb.max_len as usize).saturating_sub(1)
    }

    /// Publish a transaction crash-safely (see module docs for the four-
    /// fence ordering). On success, the in-memory JSB is up-to-date and
    /// the on-disk JSB has been written back twice (dirty marker + clean
    /// marker).
    pub fn commit(&mut self, dev: &dyn BlockDevice, tx: &Transaction) -> Result<()> {
        if !self.healthy {
            return Err(Error::Corrupt("journal writer failed; reopen for recovery"));
        }
        self.commit_inner(dev, tx)
    }

    /// This writer, keeping `needs_recovery` set after every checkpoint.
    pub(crate) fn holding_needs_recovery(mut self) -> Self {
        self.hold_needs_recovery = true;
        self
    }

    /// False once a commit failed after its first device write: the log and
    /// the final locations are then in a state only a fresh replay can judge,
    /// so this writer refuses every later commit. A refusal before any write
    /// (read-only device, stale sequence, oversized transaction) leaves the
    /// device untouched and the writer usable.
    pub fn is_healthy(&self) -> bool {
        self.healthy
    }

    fn commit_inner(&mut self, dev: &dyn BlockDevice, tx: &Transaction) -> Result<()> {
        if !dev.is_writable() {
            return Err(Error::ReadOnly);
        }

        // Sanity: the transaction's seq must match what we handed out from
        // begin(). A mismatch means the caller built it with a stale
        // writer or skipped begin() — either is a programming error that
        // could corrupt the journal.
        if tx.sequence != self.jsb.sequence {
            return Err(Error::Corrupt(
                "journal_writer: transaction sequence does not match writer state",
            ));
        }

        // The superblock block, if journaled, keeps `needs_recovery` set
        // until step 4 clears it (#228) -- in every write to it, since a
        // transaction may carry more than one and step 3 applies them all
        // in order.
        let sb_block = self.sb_block();
        let patched;
        let tx = if tx.writes.iter().any(|w| w.fs_block == sb_block) {
            let mut copy = tx.clone();
            let off = self.sb_offset_in_block();
            for w in copy.writes.iter_mut().filter(|w| w.fs_block == sb_block) {
                crate::journal_apply::set_needs_recovery_in(&mut w.bytes[off..off + 1024], true);
            }
            patched = copy;
            &patched
        } else {
            tx
        };

        // With every checksum the journal declares: a zero there is a
        // transaction Linux's recovery stops at (#80).
        let blocks = tx.commit_for(&self.jsb)?;
        if blocks.is_empty() {
            return Ok(()); // empty transaction is a no-op
        }
        if blocks.len() > self.max_blocks_per_transaction() {
            return Err(Error::Corrupt(
                "journal_writer: transaction too large for journal",
            ));
        }

        // From the first write on, a failure leaves the device in a state
        // only a fresh replay can judge; cleared again once step 4 is done.
        self.healthy = false;

        // -- Step 1: write transaction blocks to journal at logical [1..1+N).
        //    Block 0 is the JBD2 superblock; we never overwrite it here.
        let txn_first_jblock = 1usize;
        for (i, block) in blocks.iter().enumerate() {
            let jblock_idx = txn_first_jblock + i;
            let phys = self.physical_map[jblock_idx];
            // The journal inode's extent tree says where its blocks
            // are, and it is metadata off the disk like any other. The
            // replay side checks this (`journal_apply::byte_offset_of`);
            // the writer did not, so a journal inode mapping logical
            // block 1 to fs block 0 put the first journalled operation's
            // descriptor over the ext4 superblock.
            let at = crate::journal_apply::byte_offset_in(
                phys,
                self.blocks_count,
                self.block_size,
                dev.size_bytes(),
            )?;
            dev.write_at(at, block)?;
        }
        crate::journal_apply::write_needs_recovery(dev, true)?;
        dev.flush()?;

        // -- Step 2: mark journal dirty. start = first txn block; sequence
        //    unchanged so the walker matches our header_sequence.
        self.jsb.start = txn_first_jblock as u32;
        self.write_jsb(dev)?;
        dev.flush()?;

        // -- Step 3: apply writes to final-location fs blocks.
        for w in &tx.writes {
            let at = crate::journal_apply::byte_offset_in(
                w.fs_block,
                self.blocks_count,
                self.block_size,
                dev.size_bytes(),
            )?;
            dev.write_at(at, &w.bytes)?;
        }
        dev.flush()?;

        // -- Step 4: mark journal clean + advance sequence.
        self.jsb.start = 0;
        self.jsb.sequence = self.jsb.sequence.wrapping_add(1);
        self.write_jsb(dev)?;
        if !self.hold_needs_recovery {
            crate::journal_apply::write_needs_recovery(dev, false)?;
        }
        dev.flush()?;

        self.healthy = true;
        Ok(())
    }

    /// The fs block holding the primary superblock: 0, or 1 at 1 KiB.
    fn sb_block(&self) -> u64 {
        crate::superblock::SUPERBLOCK_OFFSET / u64::from(self.block_size)
    }

    fn sb_offset_in_block(&self) -> usize {
        (crate::superblock::SUPERBLOCK_OFFSET % u64::from(self.block_size)) as usize
    }

    /// Re-emit the JBD2 superblock at journal logical block 0 from the
    /// in-memory `self.jsb`. Patches only the fields we mutate
    /// (`s_start`, `s_sequence`, `s_header.h_sequence`); leaves all other
    /// bytes (including the v2 checksum trailer) intact by reading the
    /// existing block first.
    ///
    /// Big-endian on disk per JBD2 convention.
    fn write_jsb(&self, dev: &dyn BlockDevice) -> Result<()> {
        let bs_u64 = self.block_size as u64;
        let phys = self.physical_map[0];
        let mut buf = vec![0u8; self.block_size as usize];
        dev.read_at(phys * bs_u64, &mut buf)?;

        // Verify what we read still looks like our journal sb. A bit-flip
        // here would silently brick the journal; better to refuse.
        let magic = u32::from_be_bytes(buf[0x00..0x04].try_into().unwrap());
        if magic != JBD2_MAGIC_NUMBER {
            return Err(Error::Corrupt(
                "journal_writer: jsb block lost its magic between mount and commit",
            ));
        }
        let block_type = u32::from_be_bytes(buf[0x04..0x08].try_into().unwrap());
        if block_type != self.jsb.block_type {
            return Err(Error::Corrupt(
                "journal_writer: jsb block_type changed since mount",
            ));
        }

        // h_sequence (header) at 0x08, s_sequence at 0x18, s_start at 0x1C.
        // Kernel keeps h_sequence == s_sequence on the sb; we mirror that.
        buf[0x08..0x0C].copy_from_slice(&self.jsb.sequence.to_be_bytes());
        buf[0x18..0x1C].copy_from_slice(&self.jsb.sequence.to_be_bytes());
        buf[0x1C..0x20].copy_from_slice(&self.jsb.start.to_be_bytes());

        // Recompute the JBD2 superblock checksum (s_checksum at 0xFC) when the
        // journal is checksummed (CSUM_V2/V3). It covers the whole 1024-byte
        // journal_superblock_t with s_checksum zeroed, seeded ~0 — the journal
        // is self-contained, so this is NOT the fs metadata_csum seed. Leaving
        // it stale after mutating s_start/s_sequence makes a real kernel and
        // e2fsck refuse the journal ("Journal superblock is corrupt"). v1
        // journals carry no such field; their bytes are left intact.
        if self.jsb.uses_csum_v2_or_v3() {
            const JSB_LEN: usize = 1024; // sizeof(journal_superblock_t)
            buf[0xFC..0x100].copy_from_slice(&0u32.to_be_bytes());
            let csum = crate::checksum::linux_crc32c(!0, &buf[..JSB_LEN]);
            buf[0xFC..0x100].copy_from_slice(&csum.to_be_bytes());
        }

        dev.write_at(phys * bs_u64, &buf)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_journal_must_fit_its_inode_and_the_device() {
        // 8 blocks in the inode, 16 on the device.
        assert!(journal_len_fits(8, 8 * 4096, 4096, 16 * 4096).is_ok());
        assert!(journal_len_fits(9, 8 * 4096, 4096, 16 * 4096).is_err());
        // An inode claiming more than the device holds does not raise the
        // device's ceiling.
        assert!(journal_len_fits(17, u64::MAX, 4096, 16 * 4096).is_err());
        assert!(journal_len_fits(16, u64::MAX, 4096, 16 * 4096).is_ok());
        assert!(journal_len_fits(u32::MAX, u64::MAX, 4096, 64 << 20).is_err());
    }

    /// Whether a volume has a journal decides what `open` returns, and
    /// the answer is checked in both directions on volumes this crate
    /// formats itself (#265).
    ///
    /// The test this replaces copied `ext4-no-csum.img`, called `open`,
    /// and threw the answer away — `Some` and `None` both passed, and
    /// the comment above it said the image "is built without a journal
    /// *in some configs*", which nothing checked either. So the one
    /// behaviour its name claimed was untested: a `JournalWriter` handed
    /// back for a volume with no journal would have kept it green while
    /// every write through it went to blocks that are not a journal.
    ///
    /// Formatting here rather than reading a fixture is what makes the
    /// case certain, and it runs everywhere — no fixtures, no VM, no
    /// tools.
    mod journal_presence {
        use super::*;
        use crate::features::{Compat, FsFlavor};
        use std::sync::{Arc, Mutex};

        const BS: u32 = 1024;
        /// Room for the 1024-block journal an ext3 format lays down,
        /// plus its metadata.
        const VOL: u64 = 8 * 1024 * 1024;

        struct MemDev {
            bytes: Mutex<Vec<u8>>,
        }

        impl MemDev {
            fn new() -> Arc<Self> {
                Arc::new(Self {
                    bytes: Mutex::new(vec![0u8; VOL as usize]),
                })
            }
        }

        impl crate::block_io::BlockDevice for MemDev {
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
                let b = self.bytes.lock().unwrap();
                let (start, end) = (offset as usize, offset as usize + buf.len());
                if end > b.len() {
                    return Err(Error::Corrupt("MemDev: read past end"));
                }
                buf.copy_from_slice(&b[start..end]);
                Ok(())
            }
            fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
                let mut b = self.bytes.lock().unwrap();
                let (start, end) = (offset as usize, offset as usize + buf.len());
                if end > b.len() {
                    return Err(Error::Corrupt("MemDev: write past end"));
                }
                b[start..end].copy_from_slice(buf);
                Ok(())
            }
            fn size_bytes(&self) -> u64 {
                VOL
            }
            fn flush(&self) -> Result<()> {
                Ok(())
            }
            fn is_writable(&self) -> bool {
                true
            }
        }

        fn formatted(flavor: FsFlavor) -> Filesystem {
            let dev = MemDev::new();
            crate::mkfs::format_filesystem_with_flavor(dev.as_ref(), None, None, VOL, BS, flavor)
                .expect("format");
            Filesystem::mount(dev).expect("mount")
        }

        /// A volume with no journal has no journal writer.
        #[test]
        fn open_returns_none_when_the_volume_has_no_journal() {
            let fs = formatted(FsFlavor::Ext4);
            // THE FIXTURE'S OWN SHAPE FIRST. A test that asserts `None`
            // against a volume that turned out to have a journal is
            // asserting nothing about the case it names.
            assert_eq!(
                fs.sb.feature_compat & Compat::HAS_JOURNAL.bits(),
                0,
                "this volume was formatted without a journal and says it has one"
            );
            assert_eq!(fs.sb.journal_inode, 0, "and names no journal inode");

            let writer = JournalWriter::open(&fs).expect("open");
            assert!(
                writer.is_none(),
                "a volume with no journal handed back a journal writer, so every write \
                 through it would go to blocks that are not a journal"
            );
        }

        /// And one with a journal has one — or the test above would pass
        /// on an `open` that never returns anything.
        #[test]
        fn open_returns_a_writer_when_the_volume_has_one() {
            let fs = formatted(FsFlavor::Ext3);
            assert_ne!(
                fs.sb.feature_compat & Compat::HAS_JOURNAL.bits(),
                0,
                "this volume was formatted with a journal and says it has none"
            );
            assert_ne!(fs.sb.journal_inode, 0, "and it names the journal inode");

            let writer = JournalWriter::open(&fs).expect("open");
            assert!(
                writer.is_some(),
                "a volume with a journal handed back nothing, so a caller would fall \
                 back to unjournaled writes on a filesystem that has a log"
            );
        }

        /// A transaction refused before any journal block is written leaves
        /// nothing on the device to recover from, so it must not retire the
        /// writer: only a commit whose I/O began and failed does.
        #[test]
        fn a_refusal_before_any_write_leaves_the_writer_usable() {
            let fs = formatted(FsFlavor::Ext3);
            let mut writer = JournalWriter::open(&fs).expect("open").expect("writer");
            let block = fs.sb.blocks_count - 1;
            let mut too_large = writer.begin();
            for i in 0..=writer.max_blocks_per_transaction() as u64 {
                too_large
                    .add_write(block - i, vec![0u8; BS as usize])
                    .expect("add_write");
            }
            assert!(writer.commit(fs.dev.as_ref(), &too_large).is_err());
            assert!(
                writer.is_healthy(),
                "a transaction refused before it wrote anything retired the writer"
            );
            let mut small = writer.begin();
            small
                .add_write(block, vec![0u8; BS as usize])
                .expect("add_write");
            writer
                .commit(fs.dev.as_ref(), &small)
                .expect("the next transaction commits");
        }

        /// Drops every write after the first `budget`: a power cut.
        struct CutDevice {
            inner: Arc<MemDev>,
            budget: Mutex<usize>,
        }

        impl crate::block_io::BlockDevice for CutDevice {
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
                self.inner.read_at(offset, buf)
            }
            fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
                let mut left = self.budget.lock().unwrap();
                if *left == 0 {
                    return Ok(());
                }
                *left -= 1;
                self.inner.write_at(offset, buf)
            }
            fn size_bytes(&self) -> u64 {
                VOL
            }
            fn flush(&self) -> Result<()> {
                Ok(())
            }
            fn is_writable(&self) -> bool {
                true
            }
        }

        /// A data block that begins with the journal magic goes into the log
        /// escaped, and this crate's replay puts the magic back (#292).
        ///
        /// The commit is cut once the journal is marked dirty, so the target
        /// holds only what replay gives it.
        #[test]
        fn a_block_starting_with_the_magic_is_logged_escaped_and_replayed_whole() {
            let dev = MemDev::new();
            crate::mkfs::format_filesystem_with_flavor(
                dev.as_ref(),
                None,
                None,
                VOL,
                BS,
                FsFlavor::Ext3,
            )
            .expect("format");
            let fs = Filesystem::mount(dev.clone()).expect("mount");
            let mut writer = JournalWriter::open(&fs).expect("open").expect("writer");
            let target = fs.sb.blocks_count - 1;
            let mut payload: Vec<u8> = (0..BS as usize).map(|j| 0x5A ^ (j % 251) as u8).collect();
            payload[0..4].copy_from_slice(&JBD2_MAGIC_NUMBER.to_be_bytes());

            let mut tx = writer.begin();
            tx.add_write(target, payload.clone()).expect("add_write");
            // Descriptor, data, commit; `needs_recovery`; the dirty journal
            // superblock. The final-location write is lost.
            let cut = CutDevice {
                inner: dev.clone(),
                budget: Mutex::new(3 + 2),
            };
            writer.commit(&cut, &tx).expect("commit");
            let logged_at = writer.physical_map[2];
            drop(fs);

            let read = |block: u64| {
                let mut b = vec![0u8; BS as usize];
                dev.read_at(block * BS as u64, &mut b).unwrap();
                b
            };
            let logged = read(logged_at);
            assert_eq!(
                &logged[0..4],
                &[0u8; 4],
                "the logged copy starts with the journal magic, unescaped"
            );
            assert!(logged[4..] == payload[4..]);
            assert_eq!(read(target), vec![0u8; BS as usize], "cut too late");

            // A writable mount replays a dirty journal (`replay_if_dirty`).
            let fs = Filesystem::mount(dev.clone()).expect("remount");
            assert!(
                crate::jbd2::read_superblock(&fs)
                    .expect("journal superblock")
                    .expect("a journal")
                    .is_clean(),
                "the remount did not replay the journal"
            );
            drop(fs);
            assert!(
                read(target) == payload,
                "replay did not restore the block, magic and all"
            );
        }
    }

    /// Tests that copy a fixture from `test-disks/` (`chore fixtures`).
    mod needs_host {
        use super::*;
        use crate::block_io::FileDevice;
        use std::fs;
        use std::sync::Arc;

        fn copy_to_tmp(name: &str, tag: &str) -> String {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let src = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), name);
            let dst =
                fs_ext4_test_support::temp_path!("fs_ext4_jw_{}_{tag}_{n}.img", std::process::id());
            fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy {src} to {dst}: {e}"));
            dst
        }

        #[test]
        fn empty_transaction_is_no_op() {
            let path = copy_to_tmp("ext4-basic.img", "empty_tx");
            let dev = FileDevice::open_rw(&path).expect("open rw");
            let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
            let mut jw = JournalWriter::open(&fs).expect("open writer").expect(
                "ext4-basic.img is built with has_journal (test-disks/guest-build-images.sh)",
            );
            let initial_seq = jw.jsb.sequence;
            let tx = jw.begin();
            // commit() short-circuits on tx.commit() returning a single commit
            // block — actually tx.commit() always returns at least the commit
            // block, so an empty tx still goes through the protocol but writes
            // only one block. Verify it advances sequence by 1.
            jw.commit(fs.dev.as_ref(), &tx).expect("commit");
            assert_eq!(
                jw.jsb.sequence,
                initial_seq.wrapping_add(1),
                "sequence should advance even for a no-write commit"
            );
            assert_eq!(jw.jsb.start, 0, "should be clean after commit");
            fs::remove_file(path).ok();
        }
    }
}
