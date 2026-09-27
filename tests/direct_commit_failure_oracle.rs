//! #319, read back by the independent checker in the harness VM.
//!
//! `direct_commit_failure.rs` pins the mechanism: a direct commit whose last
//! write (the superblock) fails poisons the mount, so the next write is
//! refused. This file asks something that is not this driver what that
//! leaves on disk. Before the fix the mount stayed writable, still
//! believed the woken group uninit, and gave `/spread/g` the very blocks
//! `/spread/f` had just taken on disk: the checker reported them
//! multiply-claimed.
//!
//! What remains after the fix is a superblock whose free counts predate the
//! failed commit, which the checker recounts without calling it an error;
//! the group descriptors, bitmaps and inodes all agree.

mod direct_commit;

use direct_commit::{run, NEVER};
use fs_ext4_test_support::assert_e2fsck_clean;

#[test]
fn a_failed_superblock_write_leaves_no_block_claimed_twice() {
    let clean = run(NEVER);
    let fail_at = clean
        .commit_writes
        .iter()
        .position(|&block| block == 0)
        .expect("the commit writes the superblock");

    // The second write's result is not asserted here: that is the unit
    // test's job, and this one's is to hand the checker whatever the mount
    // left, refused or not.
    let failed = run(fail_at);
    assert!(failed.first.is_err(), "the injected failure was not hit");

    let image = fs_ext4_test_support::temp_path!(
        "fs_ext4_direct_commit_failure_{}.img",
        std::process::id()
    );
    std::fs::write(&image, failed.dev.bytes_snapshot()).expect("write image");
    assert_e2fsck_clean(&image, "superblock write failed");
    std::fs::remove_file(&image).expect("remove image");
}
