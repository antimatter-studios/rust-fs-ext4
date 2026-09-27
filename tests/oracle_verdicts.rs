//! THE VERDICT READER, AGAINST REPORTS THE ORACLE TOOLS REALLY PRINTED (#280).
//!
//! `tests/oracle-reports/` holds reports captured verbatim from the tools
//! in the fs-linux-test-harness guest (e2fsprogs 1.47.0, and the lwext4
//! reporter built at the pinned revision), each as
//!
//! ```text
//! exit <status>
//! --- stdout
//! <stdout, byte for byte>
//! --- stderr
//! <stderr, byte for byte>
//! ```
//!
//! Every one that is not a clean verdict must be refused as clean — and
//! several of them exited 0, which is how a suite that reads the exit
//! status came to call them clean. The few reports this file builds by
//! hand instead (a signal, a malformed line) are marked as constructed
//! where they are made.
//!
//! No VM, no tool, no image: this is the unit tier. The same rules are
//! applied to live tool output by `tests/oracle_verdicts_live.rs`.

use std::path::{Path, PathBuf};

use fs_ext4_test_support::{Judge, Verdict};

/// What a report must be read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    Clean,
    Findings,
    NotAVerdict,
}

/// Every captured report, the tool that printed it, and what it says.
///
/// The comment on each is what was done to the image before the tool was
/// asked about it.
const CAPTURED: &[(&str, Judge, Expect)] = &[
    // e2fsck -fn on a fresh mkfs.ext4 volume holding one file.
    ("e2fsck_good", Judge::E2fsck, Expect::Clean),
    // e2fsck -fn on a volume this crate wrote
    // (`deep_dir_extent_nodes_are_distinct`): `extent tree (at level 1)
    // could be narrower. Optimize? no`, exit 0. A suggestion about a valid
    // tree, which e2fsck does not count as an error either.
    ("e2fsck_optimize", Judge::E2fsck, Expect::Clean),
    // e2fsck -n (no -f) on the fresh volume: `clean, 12/4096 files`, exit
    // 0, and not one pass run.
    ("e2fsck_noforce", Judge::E2fsck, Expect::NotAVerdict),
    // `feature needs_recovery` set: `skipping journal recovery because
    // doing a read-only filesystem check`, then the passes, exit 0.
    ("e2fsck_needs_recovery", Judge::E2fsck, Expect::NotAVerdict),
    // Group 0's descriptor checksum overwritten: `One or more block group
    // descriptor checksums are invalid. Fix? no`, exit 0.
    ("e2fsck_gdt_ignored", Judge::E2fsck, Expect::Findings),
    // `ssv free_blocks_count 1`: `Free blocks count wrong (1,
    // counted=2805). Fix? no`, exit 0.
    ("e2fsck_free_count", Judge::E2fsck, Expect::Findings),
    // The same volume under -fy: repaired, exit 1.
    ("e2fsck_fy", Judge::E2fsck, Expect::Findings),
    // The root's link count set to 7: exit 4, `still has errors`.
    ("e2fsck_links", Judge::E2fsck, Expect::Findings),
    // An inode's checksum overwritten: exit 4.
    ("e2fsck_inode_csum", Judge::E2fsck, Expect::Findings),
    // The superblock checksum overwritten: e2fsck will not open it (exit
    // 8), and says it is the checksum. It looked, and that is a finding.
    ("e2fsck_sb_csum", Judge::E2fsck, Expect::Findings),
    // An unknown incompat feature: `Get a newer version of e2fsck!`.
    ("e2fsck_unsupported", Judge::E2fsck, Expect::NotAVerdict),
    // A file of zeroes: `Bad magic number in super-block`.
    ("e2fsck_zeros", Judge::E2fsck, Expect::NotAVerdict),
    // A path that names nothing.
    ("e2fsck_missing", Judge::E2fsck, Expect::NotAVerdict),
    // `ssv rev_level 9`: `Filesystem revision too high`.
    ("e2fsck_rev", Judge::E2fsck, Expect::NotAVerdict),
    // An unknown option: exit 16.
    ("e2fsck_usage", Judge::E2fsck, Expect::NotAVerdict),
    // The journal inode's mode and first block cleared: `Cannot proceed
    // with file system check`, exit 12.
    ("e2fsck_bad_journal", Judge::E2fsck, Expect::NotAVerdict),
    // debugfs -R "cat /f" on the good volume.
    ("debugfs_ok", Judge::Debugfs, Expect::Clean),
    // debugfs exits 0 on every one of these.
    ("debugfs_not_found", Judge::Debugfs, Expect::NotAVerdict),
    ("debugfs_not_dir", Judge::Debugfs, Expect::NotAVerdict),
    ("debugfs_exists", Judge::Debugfs, Expect::NotAVerdict),
    ("debugfs_zeros", Judge::Debugfs, Expect::NotAVerdict),
    ("debugfs_unsupported", Judge::Debugfs, Expect::NotAVerdict),
    // A `-f` script whose second request names nothing.
    ("debugfs_script", Judge::Debugfs, Expect::NotAVerdict),
    // An inode checksum overwritten: `Inode checksum does not match
    // inode`, exit 0 — a statement about the volume.
    ("debugfs_inode_csum", Judge::Debugfs, Expect::Findings),
    ("debugfs_inode_csum_cat", Judge::Debugfs, Expect::Findings),
    // debugfs does not check the superblock checksum: it read the volume.
    ("debugfs_sb_csum", Judge::Debugfs, Expect::Clean),
    ("dumpe2fs_ok", Judge::Dumpe2fs, Expect::Clean),
    // Group 0: `csum 0x1234 (EXPECTED 0xd2c4)`, exit 0.
    ("dumpe2fs_gdt_bad", Judge::Dumpe2fs, Expect::Findings),
    // exit 152, `Superblock checksum does not match superblock`.
    ("dumpe2fs_sb_csum", Judge::Dumpe2fs, Expect::Findings),
    ("dumpe2fs_zeros", Judge::Dumpe2fs, Expect::NotAVerdict),
    ("dumpe2fs_unsupported", Judge::Dumpe2fs, Expect::NotAVerdict),
    ("lwext4report_good", Judge::Lwext4, Expect::Clean),
    // lwext4 does not check inode checksums on a read-only mount.
    ("lwext4report_icsum", Judge::Lwext4, Expect::Clean),
    // `ext4_mount (read-only): 95 (Operation not supported)`: a refusal.
    ("lwext4report_unsupported", Judge::Lwext4, Expect::Findings),
    ("lwext4report_zeros", Judge::Lwext4, Expect::Findings),
    // `lwext4-report: unknown mode bogus`: it never asked.
    ("lwext4report_usage", Judge::Lwext4, Expect::NotAVerdict),
    // A path that names nothing: `ext4_mount (read-only): 5
    // (Input/output error)`. FROM THE TEXT ALONE THIS IS A REFUSAL, and
    // it is exactly what a refusal of a real volume prints. The support
    // crate checks that the image exists before it asks lwext4 anything;
    // the lwext4 cross-validation suite holds it to that.
    ("lwext4report_missing", Judge::Lwext4, Expect::Findings),
];

fn reports() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/oracle-reports")
}

/// `(exit status, stdout, stderr)` of one captured report.
fn captured(name: &str) -> (Option<i32>, String, String) {
    let path = reports().join(format!("{name}.txt"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let (head, rest) = text
        .split_once("\n--- stdout\n")
        .unwrap_or_else(|| panic!("{name}: no stdout marker"));
    let code = head
        .strip_prefix("exit ")
        .and_then(|c| c.trim().parse().ok())
        .unwrap_or_else(|| panic!("{name}: no exit status"));
    let (stdout, stderr) = rest
        .split_once("--- stderr\n")
        .unwrap_or_else(|| panic!("{name}: no stderr marker"));
    (Some(code), stdout.to_string(), stderr.to_string())
}

fn kind(verdict: &Verdict) -> Expect {
    match verdict {
        Verdict::Clean => Expect::Clean,
        Verdict::Findings(_) => Expect::Findings,
        Verdict::NotAVerdict(_) => Expect::NotAVerdict,
    }
}

#[test]
fn every_captured_report_is_read_as_what_it_says() {
    let mut wrong = Vec::new();
    for &(name, judge, expect) in CAPTURED {
        let (code, stdout, stderr) = captured(name);
        let verdict = judge.read(code, &stdout, &stderr);
        if kind(&verdict) != expect {
            wrong.push(format!("{name}: expected {expect:?}, read as {verdict:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The hole this closes, measured: each of these exited 0, so a suite
/// that read the exit status called every one of them clean.
#[test]
fn reports_that_exit_zero_without_a_clean_verdict_are_refused() {
    let exit_zero_and_not_clean = [
        ("e2fsck_noforce", Judge::E2fsck),
        ("e2fsck_needs_recovery", Judge::E2fsck),
        ("e2fsck_gdt_ignored", Judge::E2fsck),
        ("e2fsck_free_count", Judge::E2fsck),
        ("debugfs_not_found", Judge::Debugfs),
        ("debugfs_not_dir", Judge::Debugfs),
        ("debugfs_exists", Judge::Debugfs),
        ("debugfs_zeros", Judge::Debugfs),
        ("debugfs_unsupported", Judge::Debugfs),
        ("debugfs_inode_csum", Judge::Debugfs),
        ("dumpe2fs_gdt_bad", Judge::Dumpe2fs),
    ];
    for (name, judge) in exit_zero_and_not_clean {
        let (code, stdout, stderr) = captured(name);
        assert_eq!(code, Some(0), "{name} is supposed to be an exit-0 report");
        assert_ne!(
            judge.read(code, &stdout, &stderr),
            Verdict::Clean,
            "{name} exited 0 and is not a clean verdict, and was read as one"
        );
    }
}

/// NOTHING CAPTURED GOES UNREAD. A report added to the directory and not
/// to [`CAPTURED`] is one nobody checked the reader against.
#[test]
fn every_captured_report_is_listed() {
    let listed: Vec<&str> = CAPTURED.iter().map(|(name, _, _)| *name).collect();
    let mut on_disk: Vec<String> = std::fs::read_dir(reports())
        .expect("tests/oracle-reports")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter_map(|name| name.strip_suffix(".txt").map(str::to_string))
        .collect();
    on_disk.sort();
    let unlisted: Vec<&String> = on_disk
        .iter()
        .filter(|name| !listed.contains(&name.as_str()))
        .collect();
    assert!(unlisted.is_empty(), "not in CAPTURED: {unlisted:?}");
    assert_eq!(
        listed.len(),
        on_disk.len(),
        "a report is listed twice, or listed and not captured"
    );
}

/// CONSTRUCTED, not captured: the outcomes no tool would print on
/// request.
#[test]
fn reports_no_tool_prints_on_request_are_not_verdicts() {
    let e2fsck_passes = "Pass 1: Checking inodes, blocks, and sizes\n\
                         Pass 2: Checking directory structure\n\
                         Pass 3: Checking directory connectivity\n\
                         Pass 4: Checking reference counts\n\
                         Pass 5: Checking group summary information\n\
                         x.img: 11/4096 files (9.1% non-contiguous), 1291/4096 blocks\n";
    let cases: &[(&str, Judge, Option<i32>, &str, &str)] = &[
        // Killed by a signal: no status at all.
        ("e2fsck killed", Judge::E2fsck, None, e2fsck_passes, ""),
        ("debugfs killed", Judge::Debugfs, None, "", ""),
        ("dumpe2fs killed", Judge::Dumpe2fs, None, "", ""),
        ("lwext4 killed", Judge::Lwext4, None, "", ""),
        // A full set of passes behind a cancelled or broken exit status.
        (
            "e2fsck cancelled",
            Judge::E2fsck,
            Some(32),
            e2fsck_passes,
            "",
        ),
        (
            "e2fsck library",
            Judge::E2fsck,
            Some(128),
            e2fsck_passes,
            "",
        ),
        ("e2fsck unknown", Judge::E2fsck, Some(-1), e2fsck_passes, ""),
        // debugfs failing outright, which it does not do for a request.
        ("debugfs exit 1", Judge::Debugfs, Some(1), "", ""),
        // A reporter line that is not `kind<TAB>path<TAB>value`, or a kind
        // it does not print.
        (
            "lwext4 malformed",
            Judge::Lwext4,
            Some(0),
            "type\tf\tregular-file\nsomething else\n",
            "",
        ),
        (
            "lwext4 unknown kind",
            Judge::Lwext4,
            Some(0),
            "colour\tf\tblue\n",
            "",
        ),
        (
            "lwext4 complained",
            Judge::Lwext4,
            Some(0),
            "type\tf\tregular-file\n",
            "warning\n",
        ),
        (
            "lwext4 exit 1",
            Judge::Lwext4,
            Some(1),
            "type\tf\tregular-file\n",
            "",
        ),
    ];
    for &(what, judge, code, stdout, stderr) in cases {
        let verdict = judge.read(code, stdout, stderr);
        assert!(
            matches!(verdict, Verdict::NotAVerdict(_)),
            "{what}: read as {verdict:?}"
        );
    }
}

/// The tools a test can run and the readers they get. A tool that makes
/// a volume has no verdict to read.
#[test]
fn every_reporting_tool_has_a_reader_and_no_maker_does() {
    for (tool, judge) in [
        ("e2fsck", Some(Judge::E2fsck)),
        ("fsck.ext4", Some(Judge::E2fsck)),
        ("fsck.ext3", Some(Judge::E2fsck)),
        ("/opt/e2fsprogs/e2fsck", Some(Judge::E2fsck)),
        ("debugfs", Some(Judge::Debugfs)),
        ("dumpe2fs", Some(Judge::Dumpe2fs)),
        ("/repo/tmp/lwext4-report", Some(Judge::Lwext4)),
        ("mkfs.ext4", None),
        ("mke2fs", None),
        ("tune2fs", None),
        ("resize2fs", None),
    ] {
        assert_eq!(Judge::of(tool), judge, "{tool}");
    }
}
