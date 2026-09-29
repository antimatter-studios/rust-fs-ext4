//! `fs.ext4 write` and `mkdir`, judged by e2fsprogs in the harness VM:
//! the volume passes `e2fsck -fn` after the writes, `debugfs` dumps every
//! file byte-identical to what went in on stdin, and `debugfs ls -l`
//! shows the directories `mkdir` made.

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::{assert_e2fsck_clean, oracle};

/// `path` as debugfs reads it, dumped to a file beside the image.
fn debugfs_dump(image: &str, path: &str) -> Vec<u8> {
    let dumped = format!("{image}.dump");
    let _ = std::fs::remove_file(&dumped);
    oracle("debugfs")
        .args(["-R", &format!("dump {path} {dumped}"), image])
        .judged()
        .clean("debugfs dump");
    let got = std::fs::read(&dumped).unwrap_or_else(|e| panic!("debugfs dump {path}: {e}"));
    let _ = std::fs::remove_file(&dumped);
    got
}

#[test]
fn e2fsprogs_reads_back_what_fs_ext4_wrote() {
    let img = written_image("oracle-write");
    assert_e2fsck_clean(&img, "the volume fs.ext4 write and mkdir left");
    for (path, bytes) in write_cases() {
        assert!(
            debugfs_dump(&img, path) == bytes,
            "{path}: debugfs reads different bytes from those written"
        );
    }
    let listing = oracle("debugfs")
        .args(["-R", "ls -l /d", &img])
        .judged()
        .clean("debugfs ls -l /d");
    let listing = stdout(&listing);
    let e = listing
        .lines()
        .find(|l| l.trim_end().ends_with(" e"))
        .unwrap_or_else(|| panic!("debugfs ls -l /d has no e:\n{listing}"));
    assert!(e.contains("40755"), "e is not a 0755 directory: {e}");
}
