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
//! failed commit. The checker reports those (`Free blocks count wrong (46069,
//! counted=46067). Fix? no`) and exits 0; the kernel recomputes them from
//! the group descriptors at mount. They are the one thing the failed write
//! was carrying, so they are the one finding allowed: the group
//! descriptors, bitmaps and inodes must all agree.

mod direct_commit;

use direct_commit::{run, NEVER};
use fs_ext4_test_support::{oracle, Verdict};

/// What e2fsck says of the superblock's own totals, which it recounts from
/// the groups. The per-group forms (`... for group #1 ...`) are not these.
const SUPERBLOCK_TOTALS: [&str; 2] = ["Free blocks count wrong (", "Free inodes count wrong ("];

/// `e2fsck -fn` examined the volume and found nothing but stale superblock
/// totals, or the test fails with its report.
fn assert_only_stale_superblock_totals(image: &str) {
    let judged = oracle("e2fsck").args(["-fn", image]).judged();
    let code = judged.output.status.code();
    let report = judged.report();
    match judged.verdict {
        Verdict::Clean => {}
        Verdict::NotAVerdict(why) => panic!("e2fsck -fn did not examine the volume: {why}"),
        Verdict::Findings(_) => {
            assert_eq!(code, Some(0), "e2fsck -fn left errors:\n{report}");
            let lines: Vec<&str> = report.lines().map(str::trim_end).collect();
            for (i, line) in lines.iter().enumerate() {
                if !(line.ends_with("? no") || line.ends_with("? yes")) {
                    continue;
                }
                // `Fix? no` stands on a line of its own under the problem;
                // `Clear? no` after an inode problem shares its line.
                let problem = if line.split_whitespace().count() > 2 {
                    line
                } else {
                    lines[..i]
                        .iter()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or(&"")
                };
                assert!(
                    SUPERBLOCK_TOTALS.iter().any(|t| problem.starts_with(t)),
                    "e2fsck -fn found more than stale superblock totals \
                     (`{problem}`):\n{report}"
                );
            }
            for marker in ["IGNORED", "still has errors", "HTREE", "multiply-claimed"] {
                assert!(
                    !report.contains(marker),
                    "e2fsck -fn reported `{marker}`:\n{report}"
                );
            }
        }
    }
}

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
    assert_only_stale_superblock_totals(&image);
    std::fs::remove_file(&image).expect("remove image");
}
