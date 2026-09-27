//! THE VERDICT READER, AGAINST THE TOOLS THEMSELVES (#280).
//!
//! `tests/oracle_verdicts.rs` reads reports captured from these tools.
//! This asks the tools in the harness guest, today, so a new e2fsprogs
//! that words a report differently fails here rather than being read as
//! clean.
//!
//! Each volume is damaged in one way that e2fsck, debugfs or dumpe2fs
//! answers with exit 0 and a report that is not a clean verdict — the
//! reports an exit-status check used to pass.

use fs_ext4_test_support::{assert_e2fsck_clean, oracle, temp_path, Verdict};

/// A fresh 16 MiB volume with 4 KiB blocks.
fn volume(tag: &str, features: &str) -> String {
    let image = temp_path!("fs_ext4_verdict_{tag}_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(16 * 1024 * 1024))
        .unwrap();
    let made = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-O", features, &image])
        .output();
    assert!(
        made.status.success(),
        "mkfs.ext4: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    image
}

/// `debugfs -w -R <request>`, which must carry the request out.
fn debugfs_w(image: &str, request: &str) {
    oracle("debugfs")
        .args(["-w", "-R", request, image])
        .judged()
        .clean(request);
}

fn is_findings(verdict: &Verdict) -> bool {
    matches!(verdict, Verdict::Findings(_))
}

fn is_not_a_verdict(verdict: &Verdict) -> bool {
    matches!(verdict, Verdict::NotAVerdict(_))
}

/// The control: an untouched volume is clean, and `assert_e2fsck_clean`
/// passes it.
#[test]
fn an_untouched_volume_is_clean() {
    let image = volume("clean", "metadata_csum");
    let judged = oracle("e2fsck").args(["-fn", &image]).judged();
    assert_eq!(judged.verdict, Verdict::Clean, "{}", judged.report());
    assert_e2fsck_clean(&image, "untouched");
    let _ = std::fs::remove_file(&image);
}

/// `Free blocks count wrong (1, counted=…). Fix? no` — and exit 0.
#[test]
fn a_wrong_free_blocks_count_is_a_finding_though_e2fsck_exits_zero() {
    let image = volume("free", "metadata_csum");
    debugfs_w(&image, "ssv free_blocks_count 1");
    let judged = oracle("e2fsck").args(["-fn", &image]).judged();
    assert_eq!(judged.output.status.code(), Some(0), "{}", judged.report());
    assert!(is_findings(&judged.verdict), "{:?}", judged.verdict);
    let _ = std::fs::remove_file(&image);
}

/// And `assert_e2fsck_clean`, which every -fn check goes through, fails on
/// it. It passed it, before #280.
#[test]
#[should_panic(expected = "found something wrong with the volume")]
fn assert_e2fsck_clean_refuses_a_count_e2fsck_declined_to_fix() {
    let image = volume("free_assert", "metadata_csum");
    debugfs_w(&image, "ssv free_blocks_count 1");
    assert_e2fsck_clean(&image, "a wrong free blocks count");
}

/// A group descriptor checksum overwritten on a `uninit_bg` volume: `One
/// or more block group descriptor checksums are invalid. Fix? no`, exit
/// 0 — and dumpe2fs prints the checksum it expected.
#[test]
fn a_wrong_descriptor_checksum_is_a_finding_though_both_tools_exit_zero() {
    let image = volume("gdt", "^metadata_csum,uninit_bg");
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&image)
            .unwrap();
        // Group 0's descriptor is the first in block 1; bg_checksum is at
        // 0x1E within it.
        f.seek(SeekFrom::Start(4096 + 0x1e)).unwrap();
        f.write_all(&[0x34, 0x12]).unwrap();
    }
    let fsck = oracle("e2fsck").args(["-fn", &image]).judged();
    assert_eq!(fsck.output.status.code(), Some(0), "{}", fsck.report());
    assert!(is_findings(&fsck.verdict), "{:?}", fsck.verdict);
    let dump = oracle("dumpe2fs").arg(&image).judged();
    assert_eq!(dump.output.status.code(), Some(0), "{}", dump.report());
    assert!(is_findings(&dump.verdict), "{:?}", dump.verdict);
    let _ = std::fs::remove_file(&image);
}

/// `needs_recovery` set: `-n` does not replay, says so, grades the volume
/// anyway, and exits 0.
#[test]
fn a_check_that_skipped_journal_recovery_is_not_a_verdict() {
    let image = volume("recovery", "metadata_csum");
    debugfs_w(&image, "feature needs_recovery");
    let judged = oracle("e2fsck").args(["-fn", &image]).judged();
    assert_eq!(judged.output.status.code(), Some(0), "{}", judged.report());
    assert!(is_not_a_verdict(&judged.verdict), "{:?}", judged.verdict);
    let _ = std::fs::remove_file(&image);
}

/// Without `-f`, a volume marked clean gets one status line and exit 0.
#[test]
fn a_check_without_force_is_not_a_verdict() {
    let image = volume("noforce", "metadata_csum");
    let judged = oracle("e2fsck").args(["-n", &image]).judged();
    assert_eq!(judged.output.status.code(), Some(0), "{}", judged.report());
    assert!(is_not_a_verdict(&judged.verdict), "{:?}", judged.verdict);
    let _ = std::fs::remove_file(&image);
}

/// debugfs exits 0 on a request it could not carry out.
#[test]
fn a_debugfs_request_that_failed_is_not_a_verdict_though_it_exits_zero() {
    let image = volume("debugfs", "metadata_csum");
    for request in ["cat /nope", "ls -l /lost+found/nope"] {
        let judged = oracle("debugfs").args(["-R", request, &image]).judged();
        assert_eq!(judged.output.status.code(), Some(0), "{}", judged.report());
        assert!(
            is_not_a_verdict(&judged.verdict),
            "{request}: {:?}",
            judged.verdict
        );
    }
    let _ = std::fs::remove_file(&image);
}

/// And a plain `.output()` of one fails where it was made, instead of
/// handing back an empty stdout for the test to compare.
#[test]
#[should_panic(expected = "did not examine what it was asked about")]
fn a_failed_debugfs_request_fails_where_it_was_made() {
    let image = volume("debugfs_output", "metadata_csum");
    let _ = oracle("debugfs").args(["-R", "cat /nope", &image]).output();
}

/// A file that is not an ext4 volume at all.
#[test]
fn a_volume_the_tools_cannot_open_is_not_a_verdict() {
    let image = temp_path!("fs_ext4_verdict_zeros_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(16 * 1024 * 1024))
        .unwrap();
    for (tool, args) in [
        ("e2fsck", vec!["-fn"]),
        ("dumpe2fs", vec![]),
        ("debugfs", vec!["-R", "stat /"]),
    ] {
        let judged = oracle(tool).args(args).arg(&image).judged();
        assert!(
            is_not_a_verdict(&judged.verdict),
            "{tool}: {:?}",
            judged.verdict
        );
    }
    let _ = std::fs::remove_file(&image);
}

/// e2fsck's exit status is not its verdict, so it is not handed out —
/// refused before the tool is even run.
#[test]
#[should_panic(expected = "is a checker, and its exit status is not its verdict")]
fn e2fsck_is_only_reachable_through_its_verdict() {
    let _ = oracle("e2fsck").args(["-fn", "never-run.img"]).output();
}
