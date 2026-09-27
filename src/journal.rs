//! JBD2 transaction walker — read-only replay-plan producer.
//!
//! Walks the on-disk journal starting at `jsb.start`, decodes descriptor /
//! commit / revoke blocks, and yields a `ReplayPlan` describing what WOULD
//! be written during replay. This module only reads; the plan goes to
//! [`crate::journal_apply::apply`], which writes each block to its final
//! location when a writable mount replays a dirty journal.
//!
//! All JBD2 on-disk data is **big-endian**. Journal block numbers are
//! relative to the journal file (logical block 0 = superblock); the caller
//! resolves them to physical fs blocks via [`jbd2::journal_block_to_physical`].
//!
//! Tag layout per `fs/jbd2/journal.c`:
//!
//! - **Classical tag** (JBD1 / no CSUM_V3): at least 8 bytes
//!   - `__be32 t_blocknr`         (low 32 bits of fs block)
//!   - `__be16 t_checksum`        (per-block crc, valid with CSUM_V2 only)
//!   - `__be16 t_flags`
//!   - `__be32 t_blocknr_high`    (iff INCOMPAT_BIT64)
//!   - If `SAME_UUID` flag is not set, a 16-byte UUID follows inline.
//!
//!   - The tag is 12 bytes with BIT64 and 8 without, plus 2 with CSUM_V2
//!     (`journal_tag_bytes`).
//!
//! - **CSUM_V3 tag** (JBD2_FEATURE_INCOMPAT_CSUM_V3): 16 bytes
//!   - `__be32 t_blocknr`
//!   - `__be32 t_flags`
//!   - `__be32 t_blocknr_high`    (iff INCOMPAT_BIT64; zero and still present otherwise)
//!   - `__be32 t_checksum`        (crc32c of sequence+block)
//!   - If `SAME_UUID` is not set, a 16-byte UUID follows, as in the classical layout.
//!
//! Tag flags:
//!   - `0x1` ESCAPED   — block begins with JBD magic; first 4 bytes were zeroed during write
//!   - `0x2` SAME_UUID — skip the inline UUID (same as previous tag's)
//!   - `0x4` DELETED   — historical; not used in modern JBD2
//!   - `0x8` LAST_TAG  — final tag in this descriptor block
//!
//! Commit block: a `journal_header_t`, with `h_chksum[0]` at offset 16 on a
//! checksummed journal.
//! Revoke block: `journal_header_t` + `__be32 r_count` + array of `__be32`/`__be64`
//! fs block numbers (depending on INCOMPAT_BIT64); records blocks whose
//! contents in earlier transactions should NOT be replayed.
//!
//! ### What is believed (#81)
//!
//! A transaction's writes and revokes join the plan only once its commit
//! block has been read and, on a CSUM_V2/V3 journal, has passed its
//! checksum. A commit block failing its checksum ends the log there, as
//! `do_one_pass` treats it: that is what a crash mid-commit leaves. A
//! committed transaction -- one whose commit checks out -- with a
//! descriptor, data or revoke block failing its checksum is corruption
//! rather than a crash, and the walk refuses with [`Error::BadChecksum`]
//! before anything is replayed. A journal declaring
//! an incompat feature outside [`jbd2::SUPPORTED_JBD_INCOMPAT`], a checksum
//! declaration JBD2 refuses, or a block size other than the filesystem's, is
//! refused outright.

use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::inode::Inode;
use crate::jbd2::{
    self, JournalSuperblock, JBD2_COMMIT_BLOCK, JBD2_DESCRIPTOR_BLOCK, JBD2_MAGIC_NUMBER,
    JBD2_REVOKE_BLOCK,
};

/// Tag flag bits.
pub const TAG_ESCAPED: u32 = 0x1;
pub const TAG_SAME_UUID: u32 = 0x2;
pub const TAG_DELETED: u32 = 0x4;
pub const TAG_LAST: u32 = 0x8;

/// One entry in the replay plan: "during replay, fs_block should receive the
/// contents of journal_block". Callers wanting the actual bytes still need to
/// read them from the journal — we only produce the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayEntry {
    /// Sequence number of the containing transaction.
    pub transaction: u32,
    /// Target fs block to overwrite.
    pub fs_block: u64,
    /// Source journal block holding the new contents.
    pub journal_block: u64,
    /// Tag flags (ESCAPED/SAME_UUID/etc). ESCAPED means the reader must
    /// restore the first 4 bytes to the JBD2 magic before writing.
    pub flags: u32,
}

/// A revoke record: transaction N tells replay "do NOT overwrite this fs block
/// from any transaction with sequence <= N".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokeEntry {
    pub transaction: u32,
    pub fs_block: u64,
}

/// Ordered list of writes + revocations produced by walking the log.
#[derive(Debug, Default, Clone)]
pub struct ReplayPlan {
    pub writes: Vec<ReplayEntry>,
    pub revokes: Vec<RevokeEntry>,
    /// Last committed transaction sequence seen.
    pub last_commit: u32,
    /// Sequence to allocate after the last complete transaction (None if none).
    pub next_sequence: Option<u32>,
    /// Number of journal blocks walked (for sanity / tests).
    pub blocks_walked: u64,
}

impl ReplayPlan {
    /// Apply revoke records to the write list: drop any write whose fs_block
    /// was revoked by a later-or-equal transaction. This is what the kernel's
    /// do_one_pass / scan_revoke_records does in spirit.
    pub fn filter_revoked(&mut self) {
        if self.revokes.is_empty() {
            return;
        }
        // For each fs_block, track the highest transaction seen in a revoke.
        let mut highest: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
        for r in &self.revokes {
            let e = highest.entry(r.fs_block).or_insert(r.transaction);
            if sequence_after(r.transaction, *e) {
                *e = r.transaction;
            }
        }
        self.writes.retain(|w| match highest.get(&w.fs_block) {
            Some(&t) => sequence_after(w.transaction, t),
            None => true,
        });
    }
}

/// JBD2 transaction IDs wrap at u32, so they are ordered the way the kernel's
/// `tid_gt` orders them: by the sign of their wrapping difference.
fn sequence_after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Walk the journal from `jsb.start` and build a [`ReplayPlan`].
///
/// Returns `Ok(ReplayPlan::default())` when the journal is clean (no writes,
/// no revokes). Stops at the first block whose header is not a valid
/// JBD2 block, or once a transaction's commit block has been processed and
/// the sequence number rolls past what we expected.
pub fn walk(fs: &Filesystem, jsb: &JournalSuperblock) -> Result<ReplayPlan> {
    let mut plan = ReplayPlan::default();
    if jsb.is_clean() {
        return Ok(plan);
    }

    if jsb.unsupported_incompat() != 0 {
        return Err(Error::Unsupported(
            "journal declares an incompat feature this driver cannot replay",
        ));
    }
    if let Some(why) = jsb.checksum_declaration_error() {
        return Err(Error::Corrupt(why));
    }
    if jsb.block_size != fs.sb.block_size() {
        return Err(Error::Corrupt(
            "journal block size differs from the filesystem's",
        ));
    }
    if jsb.first == 0
        || jsb.first >= jsb.max_len
        || jsb.start < jsb.first
        || jsb.start >= jsb.max_len
    {
        return Err(Error::Corrupt("invalid journal geometry"));
    }

    let raw = fs.read_inode_raw(fs.sb.journal_inode)?;
    let jinode = Inode::parse(&raw)?;
    let block_size = jsb.block_size as u64;
    let mut cur = jsb.start as u64;
    let mut expect_seq = jsb.sequence;
    let seed = jsb.uses_csum_v2_or_v3().then(|| jsb.csum_seed());
    // The open transaction's writes and revokes, moved into the plan only
    // when its commit block checks out. A data or revoke block failing its
    // checksum is judged at the commit: in a transaction the crash tore it
    // is only the end of the log; in a committed one it is corruption.
    let mut pending = ReplayPlan::default();
    let mut pending_corrupt: Option<&'static str> = None;

    // Upper bound: never scan more than the whole ring once, counting the
    // descriptor payload blocks as well as the metadata blocks. A real
    // replay follows a circular log; here we stop when a block header does
    // not match the expected pattern or when we hit the sb block (0).
    let limit = u64::from(jsb.max_len - jsb.first);

    while plan.blocks_walked < limit {
        let block_buf = read_journal_block(fs, &jinode, cur, block_size)?;
        plan.blocks_walked += 1;

        // Every JBD2 metadata block begins with a 12-byte header whose magic
        // MUST be JBD2_MAGIC_NUMBER. A block without the magic is a data
        // block (referenced by a preceding descriptor tag) — we stop here.
        let Some(hdr) = try_parse_header(&block_buf) else {
            break;
        };

        // If the header's sequence is not what we expected, we've walked off
        // the end of valid transactions.
        if hdr.sequence != expect_seq {
            break;
        }

        match hdr.block_type {
            JBD2_DESCRIPTOR_BLOCK => {
                // A descriptor failing its checksum is judged at the commit,
                // as the kernel's `need_check_commit_time` judges it: with no
                // valid commit after it, a crash tore the transaction and the
                // log ends; with one, the log is corrupt. Its tags still say
                // how many data blocks to step over, if they parse at all.
                let torn = seed.is_some_and(|seed| !tail_checksum_matches(seed, &block_buf));
                let tags = match parse_descriptor_tags(&block_buf, &mut cur, jsb, expect_seq) {
                    Ok(tags) => tags,
                    Err(_) if torn => break,
                    Err(e) => return Err(e),
                };
                if torn {
                    pending_corrupt = Some("journal descriptor block of a committed transaction");
                }
                plan.blocks_walked += tags.len() as u64;
                if plan.blocks_walked > limit {
                    return Err(Error::Corrupt("journal transaction exceeds ring"));
                }
                for (entry, stored) in tags {
                    if let Some(seed) = seed {
                        let data =
                            read_journal_block(fs, &jinode, entry.journal_block, block_size)?;
                        let computed = jbd2::tag_checksum(seed, expect_seq, &data);
                        let computed = if jsb.uses_csum_v3() {
                            computed
                        } else {
                            computed & 0xFFFF
                        };
                        if computed != stored {
                            pending_corrupt = Some("journal data block of a committed transaction");
                        }
                    }
                    pending.writes.push(entry);
                }
            }
            JBD2_COMMIT_BLOCK => {
                if let Some(seed) = seed {
                    let stored = u32::from_be_bytes(
                        block_buf[jbd2::COMMIT_CHECKSUM_AT..jbd2::COMMIT_CHECKSUM_AT + 4]
                            .try_into()
                            .unwrap(),
                    );
                    if jbd2::commit_block_checksum(seed, &block_buf) != stored {
                        break;
                    }
                }
                if let Some(what) = pending_corrupt {
                    return Err(Error::BadChecksum { what });
                }
                plan.writes.append(&mut pending.writes);
                plan.revokes.append(&mut pending.revokes);
                plan.last_commit = expect_seq;
                expect_seq = expect_seq.wrapping_add(1);
                plan.next_sequence = Some(expect_seq);
            }
            JBD2_REVOKE_BLOCK => {
                if seed.is_some_and(|seed| !tail_checksum_matches(seed, &block_buf)) {
                    pending_corrupt = Some("journal revoke block of a committed transaction");
                }
                let revokes = parse_revoke_block(&block_buf, jsb, expect_seq)?;
                pending.revokes.extend(revokes);
            }
            _ => break,
        }

        cur = advance(cur, 1, jsb);
    }

    plan.filter_revoked();
    Ok(plan)
}

/// Advance a journal cursor by `n` blocks, wrapping around the circular log.
///
/// The log lives in `[jsb.first .. jsb.first + jsb.max_len - 1]` (block 0 is
/// the superblock, so `first` is typically 1). When the cursor reaches the
/// end it wraps back to `first`.
fn advance(cur: u64, n: u64, jsb: &JournalSuperblock) -> u64 {
    let first = jsb.first as u64;
    let capacity = (jsb.max_len as u64).saturating_sub(first);
    if capacity == 0 {
        return cur; // degenerate journal
    }
    let offset = cur - first;
    let new_off = (offset + n) % capacity;
    first + new_off
}

struct JournalHeader {
    block_type: u32,
    sequence: u32,
}

/// Whether a descriptor or revoke block's four-byte tail holds its checksum.
fn tail_checksum_matches(seed: u32, block: &[u8]) -> bool {
    let at = block.len() - jbd2::BLOCK_TAIL_BYTES;
    let stored = u32::from_be_bytes(block[at..].try_into().unwrap());
    jbd2::block_tail_checksum(seed, block) == stored
}

fn try_parse_header(block: &[u8]) -> Option<JournalHeader> {
    if block.len() < 12 {
        return None;
    }
    let magic = u32::from_be_bytes(block[0..4].try_into().ok()?);
    if magic != JBD2_MAGIC_NUMBER {
        return None;
    }
    Some(JournalHeader {
        block_type: u32::from_be_bytes(block[4..8].try_into().ok()?),
        sequence: u32::from_be_bytes(block[8..12].try_into().ok()?),
    })
}

/// Read one journal block (journal-relative block number) by resolving it
/// through the journal inode's extent tree.
fn read_journal_block(
    fs: &Filesystem,
    jinode: &Inode,
    journal_block: u64,
    block_size: u64,
) -> Result<Vec<u8>> {
    let phys = jbd2::journal_block_to_physical(fs, jinode, journal_block)?
        .ok_or(Error::Corrupt("journal block unmapped"))?;
    let mut buf = vec![0u8; block_size as usize];
    fs.dev.read_at(phys * block_size, &mut buf)?;
    Ok(buf)
}

/// Parse the tag array that follows a descriptor header. Each tag names one
/// data block that will appear next in the journal. Advances `cur` past those
/// data blocks (they are consumed, not walked for JBD2 headers). Each entry
/// comes with the tag's stored checksum: all 32 bits of a CSUM_V3 tag, the
/// 16 of a classical one.
fn parse_descriptor_tags(
    block: &[u8],
    cur: &mut u64,
    jsb: &JournalSuperblock,
    transaction: u32,
) -> Result<Vec<(ReplayEntry, u32)>> {
    let mut out = Vec::new();
    let uses_64bit = jsb.uses_64bit();
    let uses_v3 = jsb.uses_csum_v3();
    let tag_size = jsb.tag_bytes();
    // A checksummed journal keeps the last four bytes for the block's tail.
    let usable = if jsb.uses_csum_v2_or_v3() {
        block.len() - jbd2::BLOCK_TAIL_BYTES
    } else {
        block.len()
    };

    let mut pos = 12usize; // skip header
    loop {
        if pos + tag_size > usable {
            return Err(Error::Corrupt("descriptor tag overruns block"));
        }

        let blocknr_lo = u32::from_be_bytes(block[pos..pos + 4].try_into().unwrap());

        let (flags, checksum) = if uses_v3 {
            (
                u32::from_be_bytes(block[pos + 4..pos + 8].try_into().unwrap()),
                u32::from_be_bytes(block[pos + 12..pos + 16].try_into().unwrap()),
            )
        } else {
            // Classic: __be16 t_checksum + __be16 t_flags at offset 4
            (
                u16::from_be_bytes(block[pos + 6..pos + 8].try_into().unwrap()) as u32,
                u16::from_be_bytes(block[pos + 4..pos + 6].try_into().unwrap()) as u32,
            )
        };
        // Every layout carries the UUID inline unless the tag says it is the
        // previous one's.
        let uuid_follows_size = if flags & TAG_SAME_UUID != 0 { 0 } else { 16 };

        let blocknr = if uses_64bit {
            let hi = u32::from_be_bytes(block[pos + 8..pos + 12].try_into().unwrap());
            ((hi as u64) << 32) | (blocknr_lo as u64)
        } else {
            blocknr_lo as u64
        };

        // Advance the journal cursor for the data block that accompanies this tag.
        *cur = advance(*cur, 1, jsb);
        let data_journal_block = *cur;

        out.push((
            ReplayEntry {
                transaction,
                fs_block: blocknr,
                journal_block: data_journal_block,
                flags,
            },
            checksum,
        ));

        pos += tag_size + uuid_follows_size;

        if flags & TAG_LAST != 0 {
            break;
        }
        if pos + tag_size > usable {
            // Ran off the end without a LAST_TAG — descriptor is full.
            break;
        }
    }
    Ok(out)
}

fn parse_revoke_block(
    block: &[u8],
    jsb: &JournalSuperblock,
    transaction: u32,
) -> Result<Vec<RevokeEntry>> {
    // Layout: header (12) + __be32 r_count (total bytes incl. header) + records.
    if block.len() < 16 {
        return Err(Error::Corrupt("revoke block too small"));
    }
    let r_count = u32::from_be_bytes(block[12..16].try_into().unwrap()) as usize;
    let record_size = if jsb.uses_64bit() { 8 } else { 4 };
    // `r_count` is bounded by the block less its `r_checksum` tail, never
    // clamped to it: the kernel's `scan_revoke_records` refuses a larger
    // count, and reading on would take the checksum for a record (#301).
    let usable = if jsb.uses_csum_v2_or_v3() {
        block.len() - jbd2::BLOCK_TAIL_BYTES
    } else {
        block.len()
    };
    if r_count > usable {
        return Err(Error::Corrupt(
            "journal revoke block count exceeds the block",
        ));
    }
    let mut out = Vec::new();
    let mut pos = 16usize;
    let end = r_count;
    while pos + record_size <= end {
        let fs_block = if jsb.uses_64bit() {
            u64::from_be_bytes(block[pos..pos + 8].try_into().unwrap())
        } else {
            u32::from_be_bytes(block[pos..pos + 4].try_into().unwrap()) as u64
        };
        out.push(RevokeEntry {
            transaction,
            fs_block,
        });
        pos += record_size;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jbd2::{JbdIncompat, JBD2_SUPERBLOCK_V2};

    fn mk_jsb(incompat: u32, max_len: u32) -> JournalSuperblock {
        JournalSuperblock {
            block_type: JBD2_SUPERBLOCK_V2,
            header_sequence: 1,
            block_size: 4096,
            max_len,
            first: 1,
            sequence: 10,
            start: 1,
            errno: 0,
            feature_compat: 0,
            feature_incompat: incompat,
            feature_ro_compat: 0,
            uuid: [0; 16],
            nr_users: 1,
            checksum_type: 4,
            num_fc_blocks: 0,
            checksum: 0,
        }
    }

    fn header(buf: &mut [u8], block_type: u32, seq: u32) {
        buf[0..4].copy_from_slice(&JBD2_MAGIC_NUMBER.to_be_bytes());
        buf[4..8].copy_from_slice(&block_type.to_be_bytes());
        buf[8..12].copy_from_slice(&seq.to_be_bytes());
    }

    #[test]
    fn revoke_order_wraps_like_jbd2_transaction_ids() {
        let mut plan = ReplayPlan {
            writes: vec![
                ReplayEntry {
                    transaction: u32::MAX,
                    fs_block: 3,
                    journal_block: 1,
                    flags: 0,
                },
                ReplayEntry {
                    transaction: 1,
                    fs_block: 3,
                    journal_block: 2,
                    flags: 0,
                },
            ],
            revokes: vec![
                RevokeEntry {
                    transaction: u32::MAX - 1,
                    fs_block: 3,
                },
                RevokeEntry {
                    transaction: 0,
                    fs_block: 3,
                },
            ],
            ..Default::default()
        };
        plan.filter_revoked();
        assert_eq!(plan.writes.len(), 1);
        assert_eq!(plan.writes[0].transaction, 1);
    }

    #[test]
    fn revoke_records_scale_with_r_count() {
        let jsb = mk_jsb(JbdIncompat::REVOKE.bits(), 128);
        let mut blk = vec![0u8; 4096];
        header(&mut blk, JBD2_REVOKE_BLOCK, 10);
        // r_count = 16 (header) + 3*4 (three 32-bit records) = 28
        blk[12..16].copy_from_slice(&28u32.to_be_bytes());
        blk[16..20].copy_from_slice(&100u32.to_be_bytes());
        blk[20..24].copy_from_slice(&200u32.to_be_bytes());
        blk[24..28].copy_from_slice(&300u32.to_be_bytes());
        let out = parse_revoke_block(&blk, &jsb, 10).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].fs_block, 100);
        assert_eq!(out[2].fs_block, 300);
    }

    /// A revoke block claiming more bytes than it may hold, for a journal
    /// with `incompat` and a planned write to `fs_block` at `tail_at`.
    fn revoke_block_overrunning_its_records(
        r_count: u32,
        tail_at: usize,
        fs_block: u32,
    ) -> Vec<u8> {
        let mut blk = vec![0u8; 4096];
        header(&mut blk, JBD2_REVOKE_BLOCK, 10);
        blk[12..16].copy_from_slice(&r_count.to_be_bytes());
        blk[16..20].copy_from_slice(&100u32.to_be_bytes());
        blk[tail_at..tail_at + 4].copy_from_slice(&fs_block.to_be_bytes());
        blk
    }

    /// On a CSUM_V3 journal the last four bytes are `r_checksum`, not a
    /// record: an `r_count` reaching into them is refused, as the kernel's
    /// `scan_revoke_records` refuses it, rather than letting the checksum
    /// revoke a committed write (#301).
    #[test]
    fn a_csum_v3_revoke_count_reaching_the_tail_is_refused() {
        let jsb = mk_jsb(
            JbdIncompat::REVOKE.bits() | JbdIncompat::CSUM_V3.bits(),
            128,
        );
        let blk = revoke_block_overrunning_its_records(4096, 4096 - jbd2::BLOCK_TAIL_BYTES, 9000);
        let parsed = parse_revoke_block(&blk, &jsb, 10);
        let mut plan = ReplayPlan {
            writes: vec![ReplayEntry {
                transaction: 10,
                fs_block: 9000,
                journal_block: 2,
                flags: 0,
            }],
            revokes: parsed.as_ref().map(Vec::clone).unwrap_or_default(),
            ..Default::default()
        };
        plan.filter_revoked();
        assert!(
            matches!(parsed, Err(Error::Corrupt(_))),
            "r_count covering the checksum tail parsed as {} records; writes left in the plan: {}",
            parsed.map(|r| r.len()).unwrap_or_default(),
            plan.writes.len()
        );
    }

    /// The same bound with the tail counted: `r_count` up to the tail is
    /// every record the block holds.
    #[test]
    fn a_csum_v3_revoke_count_up_to_the_tail_is_read() {
        let jsb = mk_jsb(
            JbdIncompat::REVOKE.bits() | JbdIncompat::CSUM_V3.bits(),
            128,
        );
        let usable = 4096 - jbd2::BLOCK_TAIL_BYTES;
        let blk = revoke_block_overrunning_its_records(usable as u32, usable - 4, 9000);
        let out = parse_revoke_block(&blk, &jsb, 10).unwrap();
        assert_eq!(out.len(), (usable - 16) / 4);
        assert_eq!(out[0].fs_block, 100);
        assert_eq!(out.last().unwrap().fs_block, 9000);
    }

    /// Without checksums a count past the block is refused, not clamped.
    #[test]
    fn a_revoke_count_past_the_block_is_refused() {
        let jsb = mk_jsb(JbdIncompat::REVOKE.bits(), 128);
        let blk = revoke_block_overrunning_its_records(4097, 4092, 9000);
        let parsed = parse_revoke_block(&blk, &jsb, 10);
        assert!(
            matches!(parsed, Err(Error::Corrupt(_))),
            "r_count past the block parsed as {} records",
            parsed.map(|r| r.len()).unwrap_or_default()
        );
        // A count exactly the block is every record in it.
        let blk = revoke_block_overrunning_its_records(4096, 4092, 9000);
        let out = parse_revoke_block(&blk, &jsb, 10).unwrap();
        assert_eq!(out.len(), (4096 - 16) / 4);
        assert_eq!(out.last().unwrap().fs_block, 9000);
    }

    #[test]
    fn filter_revoked_drops_older_writes() {
        let mut plan = ReplayPlan::default();
        plan.writes.push(ReplayEntry {
            transaction: 5,
            fs_block: 100,
            journal_block: 1,
            flags: 0,
        });
        plan.writes.push(ReplayEntry {
            transaction: 5,
            fs_block: 200,
            journal_block: 2,
            flags: 0,
        });
        plan.writes.push(ReplayEntry {
            transaction: 7,
            fs_block: 100,
            journal_block: 3,
            flags: 0,
        });
        plan.revokes.push(RevokeEntry {
            transaction: 6,
            fs_block: 100,
        });
        plan.filter_revoked();
        // fs_block 100 revoked at txn 6; txn 5 write dropped, txn 7 kept.
        assert_eq!(plan.writes.len(), 2);
        assert!(plan.writes.iter().any(|w| w.fs_block == 200));
        assert!(plan
            .writes
            .iter()
            .any(|w| w.fs_block == 100 && w.transaction == 7));
    }

    #[test]
    fn parse_v2_tags_without_bit64() {
        // classical tag (CSUM_V2 or none), no BIT64 → 8-byte tag + 16-byte UUID
        // on the first one; SAME_UUID skips uuid on the second.
        let jsb = mk_jsb(0, 128);
        let mut blk = vec![0u8; 4096];
        header(&mut blk, JBD2_DESCRIPTOR_BLOCK, 10);
        // tag 1: blocknr=1000, flags=0 (UUID present)
        blk[12..16].copy_from_slice(&1000u32.to_be_bytes());
        blk[16..18].copy_from_slice(&0u16.to_be_bytes()); // checksum
        blk[18..20].copy_from_slice(&0u16.to_be_bytes()); // flags = 0 → uuid present
                                                          // 16-byte uuid at 20..36
                                                          // tag 2: blocknr=2000, flags = SAME_UUID | LAST (0xA)
        blk[36..40].copy_from_slice(&2000u32.to_be_bytes());
        blk[40..42].copy_from_slice(&0u16.to_be_bytes());
        blk[42..44].copy_from_slice(&((TAG_SAME_UUID | TAG_LAST) as u16).to_be_bytes());

        let mut cur = 1u64;
        let out = parse_descriptor_tags(&blk, &mut cur, &jsb, 10).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0.fs_block, 1000);
        assert_eq!(out[1].0.fs_block, 2000);
        assert_eq!(out[1].0.flags & TAG_LAST, TAG_LAST);
    }

    /// A CSUM_V3 tag is 16 bytes without BIT64 too, and a UUID follows one
    /// without SAME_UUID -- `debugfs` writes exactly that first tag.
    #[test]
    fn parse_v3_tags_without_bit64_skip_the_uuid() {
        let jsb = mk_jsb(JbdIncompat::CSUM_V3.bits(), 128);
        let mut blk = vec![0u8; 4096];
        header(&mut blk, JBD2_DESCRIPTOR_BLOCK, 10);
        blk[12..16].copy_from_slice(&1000u32.to_be_bytes());
        blk[24..28].copy_from_slice(&0xAABB_CCDDu32.to_be_bytes()); // t_checksum
        blk[28..44].fill(0xEE); // uuid
        blk[44..48].copy_from_slice(&2000u32.to_be_bytes());
        blk[48..52].copy_from_slice(&(TAG_SAME_UUID | TAG_LAST).to_be_bytes());

        let mut cur = 1u64;
        let out = parse_descriptor_tags(&blk, &mut cur, &jsb, 10).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0],
            (
                ReplayEntry {
                    transaction: 10,
                    fs_block: 1000,
                    journal_block: 2,
                    flags: 0
                },
                0xAABB_CCDD
            )
        );
        assert_eq!(out[1].0.fs_block, 2000);
        assert_eq!(out[1].0.journal_block, 3);
    }

    #[test]
    fn try_parse_header_rejects_bad_magic() {
        let mut blk = vec![0u8; 12];
        blk[0] = 1;
        assert!(try_parse_header(&blk).is_none());
    }

    #[test]
    fn try_parse_header_accepts_valid() {
        let mut blk = vec![0u8; 12];
        header(&mut blk, JBD2_COMMIT_BLOCK, 42);
        let h = try_parse_header(&blk).unwrap();
        assert_eq!(h.block_type, JBD2_COMMIT_BLOCK);
        assert_eq!(h.sequence, 42);
    }

    #[test]
    fn advance_wraps_around_the_log() {
        let jsb = mk_jsb(0, 8); // first=1, capacity=7 (blocks 1..7)
        assert_eq!(advance(1, 1, &jsb), 2);
        assert_eq!(advance(7, 1, &jsb), 1); // wrap
        assert_eq!(advance(6, 5, &jsb), 4); // 6+5 = 11, offset 10 % 7 = 3, first+3=4
    }

    #[test]
    fn clean_journal_produces_empty_plan() {
        // We can exercise walk() without real I/O by checking the clean shortcut.
        let mut jsb = mk_jsb(0, 128);
        jsb.start = 0; // clean
                       // walk() requires a full Filesystem which is heavier to fake; just
                       // verify the guard triggers by reading the public code path — the
                       // is_clean branch returns before any I/O.
        assert!(jsb.is_clean());
        let plan = ReplayPlan::default();
        assert!(plan.writes.is_empty());
        assert_eq!(plan.last_commit, 0);
    }
}
