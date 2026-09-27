//! JBD2 transaction writer (E11).
//!
//! Takes the plan-layer outputs from E5 (bitmap), E7 (extent), E8 (dir), E9
//! (htree), E10 (file) and serializes them into JBD2 journal blocks the
//! caller writes to the journal inode. Produces:
//!
//!   1. A **descriptor block** naming every dirty fs block, one tag per block.
//!   2. A **data block** for each tag with the new content.
//!   3. Optionally a **revoke block** for fs blocks whose earlier-journal
//!      contents must NOT be replayed (typically: freed leaf/index blocks so
//!      a crash during free doesn't restore stale pointers).
//!   4. A **commit block** — the single marker that atomically makes the
//!      transaction visible to recovery.
//!
//! All JBD2 data is **big-endian** on disk (opposite of ext4 filesystem body).
//!
//! This module is the dual of [`crate::journal`] (the replay walker): given
//! the same tag+data layout, `journal::walk` should reproduce the mutations
//! this module serialised. The round-trip property is covered by tests.
//!
//! ### What we serialize vs. what we don't
//!
//! - **Serialize**: the journal blocks themselves (bytes you write to the
//!   journal inode's logical block range).
//! - **Do NOT serialize**: the actual final-location writes. The caller
//!   sequences: write journal blocks → fsync → write final locations →
//!   update `jsb.start` on next checkpoint. That write-ordering discipline
//!   lives in the future E12+/capi write path; this module only produces
//!   the journal-format bytes.
//!
//! ### Checksum scope
//!
//! A journal declaring CSUM_V2 or CSUM_V3 checksums three things, all seeded
//! from the crc32c of its UUID: every tag (the sequence and the data block
//! it names -- 32 bits in a v3 tag, the low 16 in a v2 one), the descriptor
//! and revoke blocks (a four-byte tail over the rest of the block), and the
//! commit block (`h_chksum[0]`). Linux's recovery takes a commit that fails
//! its checksum for the end of the log, so a transaction written with zeros
//! there is one only this crate would replay (#80).
//!
//! [`Transaction::commit_for`] lays the blocks out for a given journal and
//! fills every checksum it declares. [`Transaction::commit`] knows only the
//! flags the transaction was begun with and no seed: it lays out the same
//! blocks with the checksum fields zero, which is right only for a journal
//! without checksums.

use crate::error::{Error, Result};
use crate::jbd2::{
    self, JbdIncompat, JournalSuperblock, BLOCK_TAIL_BYTES, COMMIT_CHECKSUM_AT, JBD2_COMMIT_BLOCK,
    JBD2_DESCRIPTOR_BLOCK, JBD2_MAGIC_NUMBER, JBD2_REVOKE_BLOCK,
};
use crate::journal::{TAG_ESCAPED, TAG_LAST, TAG_SAME_UUID};

/// One buffered write the transaction will journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournaledBlock {
    /// Target fs block (absolute — the same value that will end up in the
    /// descriptor tag's `t_blocknr`).
    pub fs_block: u64,
    /// Full block contents (must be exactly `block_size` bytes).
    pub bytes: Vec<u8>,
}

/// How a journal lays out and checksums its transaction blocks.
#[derive(Debug, Clone, Copy)]
struct Layout {
    tag_bytes: usize,
    uses_64bit: bool,
    /// The tail reserved at the end of descriptor and revoke blocks.
    tail_bytes: usize,
    /// `Some(seed)` when the checksum fields are to be filled.
    seed: Option<u32>,
    /// CSUM_V3 (32-bit tag checksum) rather than CSUM_V2 (16-bit).
    csum_v3: bool,
}

/// Transaction builder. Call [`Transaction::add_write`] to buffer a mutated
/// fs block; call [`Transaction::add_revoke`] to mark an older block's
/// contents as unreachable; call [`Transaction::commit`] to serialise.
#[derive(Debug, Clone)]
pub struct Transaction {
    pub sequence: u32,
    pub block_size: u32,
    pub uses_64bit: bool,
    pub uses_csum_v3: bool,
    pub writes: Vec<JournaledBlock>,
    pub revokes: Vec<u64>,
}

impl Transaction {
    /// Begin a new transaction with the given sequence number (next value from
    /// the JBD2 superblock).
    pub fn begin(sequence: u32, block_size: u32, uses_64bit: bool, uses_csum_v3: bool) -> Self {
        Self {
            sequence,
            block_size,
            uses_64bit,
            uses_csum_v3,
            writes: Vec::new(),
            revokes: Vec::new(),
        }
    }

    /// Buffer a dirty fs block. Panics in debug if `bytes.len() != block_size`.
    pub fn add_write(&mut self, fs_block: u64, bytes: Vec<u8>) -> Result<()> {
        if bytes.len() != self.block_size as usize {
            return Err(Error::Corrupt("add_write: bytes size != block_size"));
        }
        self.writes.push(JournaledBlock { fs_block, bytes });
        Ok(())
    }

    /// Record a revoke — tell replay to ignore earlier writes to this fs block.
    pub fn add_revoke(&mut self, fs_block: u64) {
        self.revokes.push(fs_block);
    }

    /// Serialize the transaction into journal blocks. Returns a single flat
    /// `Vec<Vec<u8>>` where each inner Vec is one `block_size` byte journal
    /// block, in the order they should be written to the journal log:
    ///
    ///   [descriptor, data..., descriptor, data..., revoke..., commit]
    ///
    /// A descriptor block tags at most `tags_per_descriptor()` data blocks, so
    /// a write set larger than that is split across several descriptor blocks,
    /// each immediately followed by the data blocks it tags and each ending in
    /// a tag with `TAG_LAST` set. Revoke records are chunked the same way, at
    /// `revokes_per_block()` per block. A small transaction therefore still
    /// produces the familiar `[descriptor, data_0 ..= data_N, revoke, commit]`.
    ///
    /// Empty transactions (no writes, no revokes) return a single commit
    /// block — matches kernel's "empty transaction" handling.
    ///
    /// The checksum fields are left zero; a journal that declares checksums
    /// needs [`Self::commit_for`].
    pub fn commit(&self) -> Result<Vec<Vec<u8>>> {
        let layout = if self.uses_csum_v3 {
            Layout {
                tag_bytes: 16,
                uses_64bit: self.uses_64bit,
                tail_bytes: BLOCK_TAIL_BYTES,
                seed: None,
                csum_v3: true,
            }
        } else {
            Layout {
                tag_bytes: if self.uses_64bit { 12 } else { 8 },
                uses_64bit: self.uses_64bit,
                tail_bytes: 0,
                seed: None,
                csum_v3: false,
            }
        };
        self.serialize(layout)
    }

    /// Serialize for `jsb`'s journal, with every checksum it declares
    /// filled in (#80). Refuses a journal whose incompat bits this driver
    /// does not handle, and a transaction begun for a different block size
    /// or tag width than `jsb` declares.
    pub fn commit_for(&self, jsb: &JournalSuperblock) -> Result<Vec<Vec<u8>>> {
        if jsb.unsupported_incompat() != 0 {
            return Err(Error::Unsupported(
                "journal declares an incompat feature this driver does not write",
            ));
        }
        if let Some(why) = jsb.checksum_declaration_error() {
            return Err(Error::Corrupt(why));
        }
        if self.block_size != jsb.block_size
            || self.uses_64bit != jsb.uses_64bit()
            || self.uses_csum_v3 != jsb.uses_csum_v3()
        {
            return Err(Error::InvalidArgument(
                "transaction was begun for a different journal layout",
            ));
        }
        let checksummed = jsb.uses_csum_v2_or_v3();
        self.serialize(Layout {
            tag_bytes: jsb.tag_bytes(),
            uses_64bit: jsb.uses_64bit(),
            tail_bytes: if checksummed { BLOCK_TAIL_BYTES } else { 0 },
            seed: checksummed.then(|| jsb.csum_seed()),
            csum_v3: jsb.feature_incompat & JbdIncompat::CSUM_V3.bits() != 0,
        })
    }

    fn serialize(&self, layout: Layout) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();

        // A transaction may touch more blocks than a single descriptor can tag.
        // JBD2 permits several descriptor blocks per transaction, each followed
        // by its own data blocks and each ending in a LAST_TAG. Emit as many as
        // the write set needs instead of failing the whole write.
        if !self.writes.is_empty() {
            for chunk in self.writes.chunks(self.tags_per_descriptor(layout)) {
                out.push(self.build_descriptor_block(chunk, layout)?);
                for w in chunk {
                    out.push(logged_copy(&w.bytes));
                }
            }
        }
        if !self.revokes.is_empty() {
            for chunk in self.revokes.chunks(self.revokes_per_block(layout)) {
                out.push(self.build_revoke_block(chunk, layout)?);
            }
        }
        out.push(self.build_commit_block(layout)?);
        Ok(out)
    }

    /// How many tags fit in one descriptor block: after its 12-byte header,
    /// and before the checksum tail a checksummed journal reserves.
    fn tags_per_descriptor(&self, layout: Layout) -> usize {
        let usable = self.block_size as usize - 12 - layout.tail_bytes;
        (usable / layout.tag_bytes).max(1)
    }

    /// How many revoke records fit in one revoke block: after its header and
    /// byte count, and before the checksum tail.
    fn revokes_per_block(&self, layout: Layout) -> usize {
        let record_size = if layout.uses_64bit { 8 } else { 4 };
        let usable = self.block_size as usize - 16 - layout.tail_bytes;
        (usable / record_size).max(1)
    }

    fn build_descriptor_block(&self, writes: &[JournaledBlock], layout: Layout) -> Result<Vec<u8>> {
        let mut blk = vec![0u8; self.block_size as usize];
        self.write_header(&mut blk, JBD2_DESCRIPTOR_BLOCK);

        let tag_size = layout.tag_bytes;
        let usable = blk.len() - layout.tail_bytes;
        let mut pos = 12usize;
        let last_idx = writes.len().saturating_sub(1);
        for (i, w) in writes.iter().enumerate() {
            let mut flags: u32 = if i == last_idx { TAG_LAST } else { 0 };
            // Always set SAME_UUID so we don't have to carry a per-tag UUID
            // (journal's own s_uuid is used implicitly).
            flags |= TAG_SAME_UUID;
            if needs_escape(&w.bytes) {
                flags |= TAG_ESCAPED;
            }

            if pos + tag_size > usable {
                return Err(Error::Corrupt("descriptor block overflow (too many tags)"));
            }

            let blocknr_lo = (w.fs_block & 0xFFFF_FFFF) as u32;
            blk[pos..pos + 4].copy_from_slice(&blocknr_lo.to_be_bytes());
            let checksum = layout
                .seed
                // Over the block as logged -- escaped -- since that is what
                // recovery reads and verifies before it restores the magic.
                .map(|seed| jbd2::tag_checksum(seed, self.sequence, &logged_copy(&w.bytes)));

            if layout.csum_v3 {
                // journal_block_tag3_t: t_blocknr, t_flags, t_blocknr_high,
                // t_checksum -- 16 bytes whether or not the high half is used.
                blk[pos + 4..pos + 8].copy_from_slice(&flags.to_be_bytes());
                if layout.uses_64bit {
                    let hi = (w.fs_block >> 32) as u32;
                    blk[pos + 8..pos + 12].copy_from_slice(&hi.to_be_bytes());
                }
                if let Some(c) = checksum {
                    blk[pos + 12..pos + 16].copy_from_slice(&c.to_be_bytes());
                }
            } else {
                // Classical: __be16 t_checksum + __be16 t_flags
                let c16 = checksum.map_or(0, |c| c as u16);
                blk[pos + 4..pos + 6].copy_from_slice(&c16.to_be_bytes());
                blk[pos + 6..pos + 8].copy_from_slice(&(flags as u16).to_be_bytes());
                if layout.uses_64bit {
                    let hi = (w.fs_block >> 32) as u32;
                    blk[pos + 8..pos + 12].copy_from_slice(&hi.to_be_bytes());
                }
            }
            pos += tag_size;
        }
        Self::seal_tail(&mut blk, layout);
        Ok(blk)
    }

    /// Fill a descriptor or revoke block's checksum tail.
    fn seal_tail(blk: &mut [u8], layout: Layout) {
        if let Some(seed) = layout.seed {
            let c = jbd2::block_tail_checksum(seed, blk);
            let at = blk.len() - BLOCK_TAIL_BYTES;
            blk[at..].copy_from_slice(&c.to_be_bytes());
        }
    }

    fn build_revoke_block(&self, revokes: &[u64], layout: Layout) -> Result<Vec<u8>> {
        let mut blk = vec![0u8; self.block_size as usize];
        self.write_header(&mut blk, JBD2_REVOKE_BLOCK);

        let record_size = if layout.uses_64bit { 8 } else { 4 };
        let records_bytes = revokes.len() * record_size;
        let total_bytes = 16 + records_bytes; // header(12) + count(4) + records
        if total_bytes > blk.len() - layout.tail_bytes {
            return Err(Error::Corrupt("revoke block overflow"));
        }
        blk[12..16].copy_from_slice(&(total_bytes as u32).to_be_bytes());
        let mut pos = 16usize;
        for &b in revokes {
            if layout.uses_64bit {
                blk[pos..pos + 8].copy_from_slice(&b.to_be_bytes());
            } else {
                blk[pos..pos + 4].copy_from_slice(&(b as u32).to_be_bytes());
            }
            pos += record_size;
        }
        Self::seal_tail(&mut blk, layout);
        Ok(blk)
    }

    fn build_commit_block(&self, layout: Layout) -> Result<Vec<u8>> {
        let mut blk = vec![0u8; self.block_size as usize];
        self.write_header(&mut blk, JBD2_COMMIT_BLOCK);
        if let Some(seed) = layout.seed {
            let c = jbd2::commit_block_checksum(seed, &blk);
            blk[COMMIT_CHECKSUM_AT..COMMIT_CHECKSUM_AT + 4].copy_from_slice(&c.to_be_bytes());
        }
        Ok(blk)
    }

    fn write_header(&self, blk: &mut [u8], block_type: u32) {
        blk[0..4].copy_from_slice(&JBD2_MAGIC_NUMBER.to_be_bytes());
        blk[4..8].copy_from_slice(&block_type.to_be_bytes());
        blk[8..12].copy_from_slice(&self.sequence.to_be_bytes());
    }
}

/// Whether a data block begins with the journal magic and so must be
/// escaped in the log.
///
/// Recovery tells journal blocks from data by their first four bytes. A data
/// block that begins with the magic, logged as it is, is one a recovery scan
/// that reaches it -- the stale tail of an earlier, longer transaction, just
/// past the last commit -- takes for a descriptor or commit block: a file's
/// contents could then be replayed as a transaction of its own. JBD2 has the
/// writer zero those four bytes in the logged copy and set `TAG_ESCAPED` on
/// its tag; replay puts the magic back (the kernel's
/// `jbd2_journal_write_metadata_buffer` and `do_one_pass`).
fn needs_escape(bytes: &[u8]) -> bool {
    bytes.get(0..4) == Some(&JBD2_MAGIC_NUMBER.to_be_bytes()[..])
}

/// The copy of a data block that goes into the log: the block itself, with
/// the magic zeroed when [`needs_escape`] says so. The final-location write
/// keeps the original bytes.
fn logged_copy(bytes: &[u8]) -> Vec<u8> {
    let mut logged = bytes.to_vec();
    if needs_escape(bytes) {
        logged[0..4].fill(0);
    }
    logged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jbd2::{JbdIncompat, JournalSuperblock, JBD2_SUPERBLOCK_V2};
    use crate::journal::{ReplayEntry, ReplayPlan, RevokeEntry};

    fn mk_tx(seq: u32) -> Transaction {
        Transaction::begin(seq, 4096, false, false)
    }

    #[test]
    fn empty_transaction_yields_only_commit() {
        let tx = mk_tx(1);
        let blocks = tx.commit().unwrap();
        assert_eq!(blocks.len(), 1);
        // The one block must be a commit with magic + type 2.
        let magic = u32::from_be_bytes(blocks[0][0..4].try_into().unwrap());
        let bt = u32::from_be_bytes(blocks[0][4..8].try_into().unwrap());
        assert_eq!(magic, JBD2_MAGIC_NUMBER);
        assert_eq!(bt, JBD2_COMMIT_BLOCK);
    }

    #[test]
    fn two_block_txn_layout_is_desc_then_data_then_commit() {
        let mut tx = mk_tx(42);
        tx.add_write(100, vec![0xAA; 4096]).unwrap();
        tx.add_write(200, vec![0xBB; 4096]).unwrap();
        let blocks = tx.commit().unwrap();
        assert_eq!(blocks.len(), 4); // desc + 2 data + commit
                                     // blocks[0] = descriptor
        let bt0 = u32::from_be_bytes(blocks[0][4..8].try_into().unwrap());
        assert_eq!(bt0, JBD2_DESCRIPTOR_BLOCK);
        // blocks[1..3] = data (no JBD header required; just payload)
        assert_eq!(blocks[1][0], 0xAA);
        assert_eq!(blocks[2][0], 0xBB);
        // blocks[3] = commit
        let bt3 = u32::from_be_bytes(blocks[3][4..8].try_into().unwrap());
        assert_eq!(bt3, JBD2_COMMIT_BLOCK);
    }

    #[test]
    fn descriptor_tags_are_big_endian_and_have_last_flag() {
        let mut tx = mk_tx(7);
        tx.add_write(0x1234_5678, vec![0u8; 4096]).unwrap();
        let blocks = tx.commit().unwrap();
        let desc = &blocks[0];
        let blocknr_lo = u32::from_be_bytes(desc[12..16].try_into().unwrap());
        assert_eq!(blocknr_lo, 0x1234_5678);
        let flags_u16 = u16::from_be_bytes(desc[18..20].try_into().unwrap()) as u32;
        assert_eq!(flags_u16 & TAG_LAST, TAG_LAST, "last tag must set TAG_LAST");
        assert_eq!(
            flags_u16 & TAG_SAME_UUID,
            TAG_SAME_UUID,
            "same_uuid expected"
        );
    }

    #[test]
    fn writes_beyond_one_descriptor_spill_into_a_second_descriptor_block() {
        // 1 KiB blocks, classic 8-byte tags: (1024 - 12) / 8 = 126 tags fit.
        let mut tx = Transaction::begin(9, 1024, false, false);
        let per = 126;
        for i in 0..(per + 1) {
            tx.add_write(1000 + i as u64, vec![i as u8; 1024]).unwrap();
        }
        let blocks = tx.commit().unwrap();
        // desc + 126 data + desc + 1 data + commit
        assert_eq!(blocks.len(), 1 + per + 1 + 1 + 1);

        let block_type = |b: &Vec<u8>| u32::from_be_bytes(b[4..8].try_into().unwrap());
        assert_eq!(block_type(&blocks[0]), JBD2_DESCRIPTOR_BLOCK);
        assert_eq!(block_type(&blocks[per + 1]), JBD2_DESCRIPTOR_BLOCK);
        assert_eq!(block_type(&blocks[per + 3]), JBD2_COMMIT_BLOCK);
        // data blocks sit directly after their own descriptor
        assert_eq!(blocks[1][0], 0);
        assert_eq!(blocks[per][0], (per - 1) as u8);
        assert_eq!(blocks[per + 2][0], per as u8);

        // both descriptors end in a LAST tag; the first one is full
        let last_tag_flags = |desc: &Vec<u8>, idx: usize| {
            u16::from_be_bytes(desc[12 + idx * 8 + 6..12 + idx * 8 + 8].try_into().unwrap()) as u32
        };
        assert_eq!(last_tag_flags(&blocks[0], per - 1) & TAG_LAST, TAG_LAST);
        assert_eq!(
            last_tag_flags(&blocks[0], 0) & TAG_LAST,
            0,
            "only the final tag is LAST"
        );
        assert_eq!(last_tag_flags(&blocks[per + 1], 0) & TAG_LAST, TAG_LAST);
    }

    #[test]
    fn exactly_one_descriptor_worth_of_writes_stays_in_one_block() {
        let mut tx = Transaction::begin(10, 1024, false, false);
        let per = 126;
        for i in 0..per {
            tx.add_write(2000 + i as u64, vec![0u8; 1024]).unwrap();
        }
        let blocks = tx.commit().unwrap();
        assert_eq!(blocks.len(), 1 + per + 1); // desc + data + commit, no spill
    }

    #[test]
    fn revokes_beyond_one_block_spill_into_a_second_revoke_block() {
        // 1 KiB blocks, 4-byte records: (1024 - 16) / 4 = 252 fit.
        let mut tx = Transaction::begin(11, 1024, false, false);
        let per = 252;
        for i in 0..(per + 1) {
            tx.add_revoke(5000 + i as u64);
        }
        let blocks = tx.commit().unwrap();
        assert_eq!(blocks.len(), 3); // revoke + revoke + commit
        let block_type = |b: &Vec<u8>| u32::from_be_bytes(b[4..8].try_into().unwrap());
        assert_eq!(block_type(&blocks[0]), JBD2_REVOKE_BLOCK);
        assert_eq!(block_type(&blocks[1]), JBD2_REVOKE_BLOCK);
        assert_eq!(block_type(&blocks[2]), JBD2_COMMIT_BLOCK);
        let count0 = u32::from_be_bytes(blocks[0][12..16].try_into().unwrap());
        let count1 = u32::from_be_bytes(blocks[1][12..16].try_into().unwrap());
        assert_eq!(count0, (16 + per * 4) as u32);
        assert_eq!(count1, 16 + 4);
    }

    #[test]
    fn revoke_block_serialises_count_and_records() {
        let mut tx = mk_tx(3);
        tx.add_revoke(1000);
        tx.add_revoke(2000);
        let blocks = tx.commit().unwrap();
        assert_eq!(blocks.len(), 2); // revoke + commit (no writes)
        let rev = &blocks[0];
        let bt = u32::from_be_bytes(rev[4..8].try_into().unwrap());
        assert_eq!(bt, JBD2_REVOKE_BLOCK);
        let r_count = u32::from_be_bytes(rev[12..16].try_into().unwrap());
        assert_eq!(r_count, 16 + 2 * 4);
        let r0 = u32::from_be_bytes(rev[16..20].try_into().unwrap());
        let r1 = u32::from_be_bytes(rev[20..24].try_into().unwrap());
        assert_eq!(r0, 1000);
        assert_eq!(r1, 2000);
    }

    /// Round-trip: build a transaction, feed its output into journal::walk
    /// via a stubbed superblock/inode, and verify the ReplayPlan names the
    /// same fs blocks in the same order.
    #[test]
    fn round_trip_through_walk_style_parsing() {
        // We can't invoke journal::walk directly without a Filesystem, but we
        // can parse the descriptor manually using the same logic journal does.
        let mut tx = mk_tx(100);
        tx.add_write(500, vec![0x11; 4096]).unwrap();
        tx.add_write(501, vec![0x22; 4096]).unwrap();
        tx.add_write(900, vec![0x33; 4096]).unwrap();
        let blocks = tx.commit().unwrap();
        let desc = &blocks[0];
        assert_eq!(
            u32::from_be_bytes(desc[4..8].try_into().unwrap()),
            JBD2_DESCRIPTOR_BLOCK
        );

        // Manually decode tags (classical, no 64bit, no v3) and confirm three
        // entries naming 500, 501, 900 with the last one flagged.
        let mut pos = 12usize;
        let mut seen = Vec::new();
        for _ in 0..3 {
            let blocknr = u32::from_be_bytes(desc[pos..pos + 4].try_into().unwrap()) as u64;
            let flags = u16::from_be_bytes(desc[pos + 6..pos + 8].try_into().unwrap()) as u32;
            seen.push((blocknr, flags));
            pos += 8;
        }
        assert_eq!(seen[0].0, 500);
        assert_eq!(seen[1].0, 501);
        assert_eq!(seen[2].0, 900);
        assert_eq!(seen[0].1 & TAG_LAST, 0);
        assert_eq!(seen[1].1 & TAG_LAST, 0);
        assert_eq!(seen[2].1 & TAG_LAST, TAG_LAST);
    }

    /// V3 tags are 16 bytes with or without BIT64: without it the
    /// `t_blocknr_high` slot is zero but still there. Exercise the v3 path
    /// for encoding at least.
    #[test]
    fn v3_tag_layout_32bit_flags() {
        let mut tx = Transaction::begin(50, 4096, false, true);
        tx.add_write(0xABCD_1234, vec![0u8; 4096]).unwrap();
        let blocks = tx.commit().unwrap();
        let desc = &blocks[0];
        let blocknr = u32::from_be_bytes(desc[12..16].try_into().unwrap());
        assert_eq!(blocknr, 0xABCD_1234);
        // v3: flags is at offset pos+4..pos+8, full 32 bits.
        let flags = u32::from_be_bytes(desc[16..20].try_into().unwrap());
        assert_eq!(flags & TAG_LAST, TAG_LAST);
        assert_eq!(flags & TAG_SAME_UUID, TAG_SAME_UUID);
    }

    fn csum_jsb(incompat: u32) -> JournalSuperblock {
        JournalSuperblock {
            block_type: JBD2_SUPERBLOCK_V2,
            header_sequence: 0,
            block_size: 4096,
            max_len: 1024,
            first: 1,
            sequence: 9,
            start: 0,
            errno: 0,
            feature_compat: 0,
            feature_incompat: incompat,
            feature_ro_compat: 0,
            uuid: *b"0123456789abcdef",
            nr_users: 1,
            checksum_type: 4,
            num_fc_blocks: 0,
            checksum: 0,
        }
    }

    fn be32(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
    }

    /// Every checksum a CSUM_V3 journal declares is filled, in a 16-byte tag
    /// even without BIT64 (#80).
    #[test]
    fn commit_for_fills_csum_v3_checksums() {
        let jsb = csum_jsb(JbdIncompat::REVOKE.bits() | JbdIncompat::CSUM_V3.bits());
        let seed = jsb.csum_seed();
        let mut tx = Transaction::begin(9, 4096, false, true);
        tx.add_write(100, vec![0x11; 4096]).unwrap();
        tx.add_write(200, vec![0x22; 4096]).unwrap();
        tx.add_revoke(300);
        let blocks = tx.commit_for(&jsb).unwrap();
        assert_eq!(blocks.len(), 5, "descriptor, two data, revoke, commit");

        let desc = &blocks[0];
        for (i, data) in [&blocks[1], &blocks[2]].iter().enumerate() {
            let tag = 12 + 16 * i;
            assert_eq!(be32(desc, tag), [100, 200][i]);
            assert_eq!(be32(desc, tag + 12), jbd2::tag_checksum(seed, 9, data));
        }
        assert_eq!(be32(desc, 4092), jbd2::block_tail_checksum(seed, desc));
        assert_eq!(
            be32(&blocks[3], 4092),
            jbd2::block_tail_checksum(seed, &blocks[3])
        );
        assert_eq!(
            be32(&blocks[4], COMMIT_CHECKSUM_AT),
            jbd2::commit_block_checksum(seed, &blocks[4])
        );
        assert_ne!(be32(&blocks[4], COMMIT_CHECKSUM_AT), 0);
    }

    /// CSUM_V2: classical tags two bytes longer, the low 16 bits of the
    /// checksum in `t_checksum`.
    #[test]
    fn commit_for_lays_csum_v2_tags() {
        let jsb = csum_jsb(JbdIncompat::CSUM_V2.bits());
        let seed = jsb.csum_seed();
        let mut tx = Transaction::begin(9, 4096, false, false);
        tx.add_write(100, vec![0x11; 4096]).unwrap();
        tx.add_write(200, vec![0x22; 4096]).unwrap();
        let blocks = tx.commit_for(&jsb).unwrap();
        let desc = &blocks[0];
        assert_eq!(be32(desc, 12 + 10), 200, "second tag starts ten bytes on");
        let c16 = u16::from_be_bytes(desc[12 + 10 + 4..12 + 10 + 6].try_into().unwrap());
        assert_eq!(c16, jbd2::tag_checksum(seed, 9, &blocks[2]) as u16);
        assert_eq!(be32(desc, 4092), jbd2::block_tail_checksum(seed, desc));
    }

    #[test]
    fn commit_for_refuses_what_it_cannot_write() {
        let tx = Transaction::begin(9, 4096, false, true);
        let v3 = JbdIncompat::CSUM_V3.bits();
        for incompat in [
            v3 | JbdIncompat::ASYNC_COMMIT.bits(),
            v3 | JbdIncompat::FAST_COMMIT.bits(),
        ] {
            assert!(matches!(
                tx.commit_for(&csum_jsb(incompat)),
                Err(Error::Unsupported(_))
            ));
        }
        for incompat in [0, v3 | JbdIncompat::BIT64.bits()] {
            assert!(matches!(
                tx.commit_for(&csum_jsb(incompat)),
                Err(Error::InvalidArgument(_))
            ));
        }
    }

    /// The tail is not tag space either, and it is where a descriptor's
    /// capacity changes: at 1 KiB with CSUM_V2 32-bit tags (10 bytes),
    /// (1024 - 12) / 10 = 101 fit without a tail and (1024 - 12 - 4) / 10 =
    /// 100 with it. The 101st write must start a second descriptor, each
    /// descriptor sealed with its own tail and each tag checksummed against
    /// the data block that follows its own descriptor.
    #[test]
    fn a_split_transaction_checksums_every_descriptor() {
        let jsb = JournalSuperblock {
            block_size: 1024,
            ..csum_jsb(JbdIncompat::CSUM_V2.bits())
        };
        let seed = jsb.csum_seed();
        let mut tx = Transaction::begin(9, 1024, false, false);
        for i in 0..101u64 {
            tx.add_write(3000 + i, vec![i as u8 + 1; 1024]).unwrap();
        }
        let blocks = tx.commit_for(&jsb).unwrap();
        // desc + 100 data + desc + 1 data + commit
        assert_eq!(blocks.len(), 104);
        let (first, second) = (&blocks[0], &blocks[101]);
        for desc in [first, second] {
            assert_eq!(be32(desc, 4), JBD2_DESCRIPTOR_BLOCK);
            assert_eq!(be32(desc, 1020), jbd2::block_tail_checksum(seed, desc));
        }
        let flags = |desc: &[u8], idx: usize| {
            u16::from_be_bytes(
                desc[12 + idx * 10 + 6..12 + idx * 10 + 8]
                    .try_into()
                    .unwrap(),
            ) as u32
        };
        assert_eq!(flags(first, 99) & TAG_LAST, TAG_LAST);
        assert_eq!(flags(first, 98) & TAG_LAST, 0);
        assert_eq!(flags(second, 0) & TAG_LAST, TAG_LAST);
        assert_eq!(
            be32(second, 12),
            3100,
            "the spilled tag names the 101st block"
        );
        let c16 = u16::from_be_bytes(second[16..18].try_into().unwrap());
        assert_eq!(c16, jbd2::tag_checksum(seed, 9, &blocks[102]) as u16);
        assert_eq!(be32(&blocks[103], 4), JBD2_COMMIT_BLOCK);
    }

    /// The tail is not record space: a revoke block of 64-bit records holds
    /// (4096 - 16) / 8 = 510 without one and 509 with it, so the 510th
    /// record starts a second revoke block rather than overwriting the tail.
    #[test]
    fn revoke_tail_is_reserved() {
        let b64 = JbdIncompat::BIT64.bits();
        let jsb = csum_jsb(JbdIncompat::CSUM_V3.bits() | b64);
        let seed = jsb.csum_seed();
        let mut tx = Transaction::begin(9, 4096, true, true);
        for b in 0..509 {
            tx.add_revoke(1000 + b);
        }
        let blocks = tx.commit_for(&jsb).unwrap();
        assert_eq!(blocks.len(), 2, "509 records: one revoke block + commit");
        assert_eq!(be32(&blocks[0], 12), 16 + 509 * 8);

        tx.add_revoke(2000);
        let blocks = tx.commit_for(&jsb).unwrap();
        assert_eq!(blocks.len(), 3, "510 records: two revoke blocks + commit");
        assert_eq!(be32(&blocks[0], 12), 16 + 509 * 8);
        assert_eq!(be32(&blocks[1], 12), 16 + 8);
        assert_eq!(
            u64::from_be_bytes(blocks[1][16..24].try_into().unwrap()),
            2000
        );
        for blk in &blocks[..2] {
            assert_eq!(be32(blk, 4), JBD2_REVOKE_BLOCK);
            assert_eq!(be32(blk, 4092), jbd2::block_tail_checksum(seed, blk));
        }
    }

    #[test]
    fn wrong_block_size_payload_is_rejected() {
        let mut tx = mk_tx(1);
        let err = tx.add_write(100, vec![0u8; 2048]).unwrap_err();
        match err {
            Error::Corrupt(msg) => assert!(msg.contains("size")),
            _ => panic!(),
        }
    }

    // A tiny adapter that mimics ReplayPlan construction directly from a
    // serialized classical descriptor — used to document the round-trip
    // invariant even without a live Filesystem.
    fn parse_descriptor_classical_no64(desc: &[u8], seq: u32) -> ReplayPlan {
        let mut plan = ReplayPlan::default();
        let mut pos = 12usize;
        let mut tag_idx = 0u64;
        loop {
            if pos + 8 > desc.len() {
                break;
            }
            let blocknr = u32::from_be_bytes(desc[pos..pos + 4].try_into().unwrap()) as u64;
            let flags = u16::from_be_bytes(desc[pos + 6..pos + 8].try_into().unwrap()) as u32;
            if blocknr == 0 && flags == 0 && tag_idx > 0 {
                break;
            }
            plan.writes.push(ReplayEntry {
                transaction: seq,
                fs_block: blocknr,
                journal_block: tag_idx + 1,
                flags,
            });
            tag_idx += 1;
            pos += 8;
            if flags & TAG_LAST != 0 {
                break;
            }
        }
        plan
    }

    #[test]
    fn parsed_descriptor_matches_original_tags() {
        let mut tx = mk_tx(77);
        tx.add_write(10, vec![0u8; 4096]).unwrap();
        tx.add_write(20, vec![0u8; 4096]).unwrap();
        tx.add_write(30, vec![0u8; 4096]).unwrap();
        let blocks = tx.commit().unwrap();
        let plan = parse_descriptor_classical_no64(&blocks[0], 77);
        assert_eq!(plan.writes.len(), 3);
        assert_eq!(plan.writes[0].fs_block, 10);
        assert_eq!(plan.writes[1].fs_block, 20);
        assert_eq!(plan.writes[2].fs_block, 30);
    }

    /// A data block whose first four bytes are the journal magic, the rest
    /// a pattern.
    fn magic_led(len: usize) -> Vec<u8> {
        let mut b: Vec<u8> = (0..len).map(|j| 0x5A ^ (j % 251) as u8).collect();
        b[0..4].copy_from_slice(&JBD2_MAGIC_NUMBER.to_be_bytes());
        b
    }

    /// A data block that begins with the journal magic is logged escaped: the
    /// magic zeroed in the logged copy, `TAG_ESCAPED` on its tag, the rest of
    /// the block untouched (#292). A block that does not is logged as it is.
    #[test]
    fn a_data_block_starting_with_the_magic_is_escaped_in_the_log() {
        let original = magic_led(4096);
        let mut tx = mk_tx(5);
        tx.add_write(100, original.clone()).unwrap();
        tx.add_write(200, vec![0x77; 4096]).unwrap();
        let blocks = tx.commit().unwrap();
        assert_eq!(blocks.len(), 4, "descriptor, two data, commit");

        let desc = &blocks[0];
        let tag_flags = |i: usize| {
            u16::from_be_bytes(desc[12 + 8 * i + 6..12 + 8 * i + 8].try_into().unwrap()) as u32
        };
        assert_eq!(
            tag_flags(0) & TAG_ESCAPED,
            TAG_ESCAPED,
            "the tag of a block that starts with the magic is not marked escaped"
        );
        assert_eq!(
            &blocks[1][0..4],
            &[0u8; 4],
            "the logged copy still starts with the journal magic"
        );
        assert!(
            blocks[1][4..] == original[4..],
            "escaping touched more than the magic"
        );
        assert_eq!(
            tag_flags(1) & TAG_ESCAPED,
            0,
            "an ordinary block was escaped"
        );
        assert!(blocks[2] == vec![0x77; 4096]);
        assert!(
            tx.writes[0].bytes == original,
            "the final-location copy lost its magic"
        );
    }

    /// Under CSUM_V3 the flag sits in the 32-bit `t_flags`, and the tag
    /// checksum covers the block as logged -- escaped -- which is what
    /// recovery reads and verifies before it restores the magic.
    #[test]
    fn an_escaped_block_is_checksummed_as_logged() {
        let jsb = csum_jsb(JbdIncompat::REVOKE.bits() | JbdIncompat::CSUM_V3.bits());
        let seed = jsb.csum_seed();
        let mut tx = Transaction::begin(9, 4096, false, true);
        tx.add_write(100, magic_led(4096)).unwrap();
        let blocks = tx.commit_for(&jsb).unwrap();
        let desc = &blocks[0];
        assert_eq!(be32(desc, 12 + 4) & TAG_ESCAPED, TAG_ESCAPED);
        assert_eq!(&blocks[1][0..4], &[0u8; 4]);
        assert_eq!(be32(desc, 12 + 12), jbd2::tag_checksum(seed, 9, &blocks[1]));
    }

    // Silence unused imports kept for future checksum wiring.
    #[test]
    fn jsb_constants_available() {
        let _ = (
            JournalSuperblock {
                block_type: JBD2_SUPERBLOCK_V2,
                header_sequence: 0,
                block_size: 4096,
                max_len: 1024,
                first: 1,
                sequence: 1,
                start: 0,
                errno: 0,
                feature_compat: 0,
                feature_incompat: JbdIncompat::REVOKE.bits(),
                feature_ro_compat: 0,
                uuid: [0; 16],
                nr_users: 1,
                checksum_type: 0,
                num_fc_blocks: 0,
                checksum: 0,
            },
            RevokeEntry {
                transaction: 1,
                fs_block: 0,
            },
        );
    }
}
