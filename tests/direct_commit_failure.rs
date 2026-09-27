//! A direct (unjournaled) commit that fails part-way must neither leave a
//! group's uninit flag cleared over a bitmap that was never written, nor let
//! the mount carry on writing (#319).
//!
//! Without a journal, `commit_block_buffer` writes the dirty blocks itself.
//! Two things went wrong there.
//!
//! - **Order.** The blocks went out in block-number order, and the group
//!   descriptor table sits near the front of the volume, so a descriptor
//!   clearing `BLOCK_UNINIT` was written before the (higher-numbered) bitmap
//!   it vouches for. A failure in between left the flag down over a bitmap
//!   holding whatever the flag had licensed leaving there.
//! - **Poison.** Any write error returned through `?`, which skipped the
//!   step that tells the mount a group's flag is down, and the mount stayed
//!   writable. When only the final superblock write failed, the inode, the
//!   extent and the bitmap were all on disk while the mount still believed
//!   the group uninit, so the next allocation rebuilt the bitmap from the
//!   group's metadata alone and handed out the same block again.
//!
//! The device here fails exactly one chosen `write_at`, so every point in a
//! commit can be made the one that breaks.
//!
//! The scenario lives in `direct_commit/mod.rs`, shared with
//! `direct_commit_failure_oracle.rs`, which hands the same volume to the
//! independent checker.

mod direct_commit;

use direct_commit::{run, Outcome, BLOCK_SIZE, GARBAGE, NEVER};
use fs_ext4::bgd::BgdFlags;
use fs_ext4::error::Error;
use fs_ext4::superblock::EXT4_VALID_FS;

/// The clean run's commit, as the block numbers it wrote in order.
fn clean_commit() -> Vec<u64> {
    let clean = run(NEVER);
    clean.first.as_ref().expect("the clean pwrite");
    clean.second.as_ref().expect("the clean second pwrite");
    assert!(
        !clean.target_flags.contains(BgdFlags::BLOCK_UNINIT),
        "premise: the tested pwrite is the one that wakes the target group"
    );
    clean.commit_writes
}

/// Where in the clean commit the write of `block` falls.
fn index_of(block: u64) -> usize {
    let writes = clean_commit();
    writes
        .iter()
        .position(|&b| b == block)
        .unwrap_or_else(|| panic!("the commit never wrote block {block}: {writes:?}"))
}

/// Everything a failed direct commit owes, whichever write failed.
fn assert_failed_safely(o: &Outcome, fail_at: usize) {
    assert!(
        o.first.is_err(),
        "write #{fail_at} of the commit failed but the pwrite reported success"
    );
    // ORDER: a descriptor that has let go of BLOCK_UNINIT vouches for the
    // bitmap beside it, so that bitmap must already be on disk.
    if !o.target_flags.contains(BgdFlags::BLOCK_UNINIT) {
        assert!(
            o.target_bitmap != vec![GARBAGE; BLOCK_SIZE as usize],
            "write #{fail_at} failed after the descriptor cleared BLOCK_UNINIT but before \
             the block bitmap {} was written: the next mount trusts garbage \
             (commit order {:?})",
            o.bitmap_block,
            o.commit_writes
        );
    }
    // POISON: the mount no longer knows what is on disk, so it must stop.
    assert!(
        matches!(o.second, Err(Error::ReadOnly)),
        "after write #{fail_at} of a direct commit failed, the same mount wrote again \
         ({:?}) -- planning against a group it still believed uninit",
        o.second
    );
    // And it must not tell the next owner the volume was put away cleanly.
    assert_eq!(
        o.state_after_drop & EXT4_VALID_FS,
        0,
        "a mount whose direct commit failed marked the volume clean on drop"
    );
}

/// The issue's second failure point: every block of the pwrite reached the
/// disk except the superblock, which goes last. The inode, extent and
/// bitmap are down while the mount still has the group as uninit, so the
/// next allocation rebuilds its bitmap from metadata and offers the block
/// just taken.
#[test]
fn a_failed_superblock_write_poisons_the_mount() {
    let fail_at = index_of(0);
    assert_eq!(
        fail_at,
        clean_commit().len() - 1,
        "the superblock is the commit's last write"
    );
    assert_failed_safely(&run(fail_at), fail_at);
}

/// The issue's first failure point: the target group's block bitmap. In
/// block-number order the descriptor table, near the front of the volume,
/// went first, so this failure left BLOCK_UNINIT cleared over the garbage
/// the flag had licensed.
#[test]
fn a_failed_bitmap_write_leaves_the_descriptor_uninit() {
    let bitmap = run(NEVER).bitmap_block;
    let fail_at = index_of(bitmap);
    assert_failed_safely(&run(fail_at), fail_at);
}

/// Every other point in the commit, so an order that is right at the two
/// named ones and wrong between them cannot pass.
#[test]
fn a_failure_at_any_write_of_a_direct_commit_is_contained() {
    let writes = clean_commit();
    assert!(writes.len() >= 4, "the commit this sweeps: {writes:?}");
    for fail_at in 0..writes.len() {
        assert_failed_safely(&run(fail_at), fail_at);
    }
}

/// The ordering, named: the target group's descriptor goes out after its
/// bitmap, and the superblock after both.
#[test]
fn bitmaps_precede_the_descriptor_that_clears_uninit() {
    let clean = run(NEVER);
    let writes = &clean.commit_writes;
    let at = |b: u64| writes.iter().position(|&w| w == b);
    let bitmap = at(clean.bitmap_block).expect("the bitmap was written");
    let descriptor = at(1).expect("the descriptor table was written");
    let superblock = at(0).expect("the superblock was written");
    assert!(
        bitmap < descriptor && descriptor < superblock,
        "commit order {writes:?}: bitmap {} must precede descriptor block 1, \
         which must precede superblock block 0",
        clean.bitmap_block
    );
}
