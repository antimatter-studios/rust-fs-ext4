//! `mkfs.ext4`'s output (the multi-call binary run under that name),
//! checked by e2fsprogs.
//!
//! This was the `validate-mkfs-bin` CI job (and `validate-fsck` in
//! release.yml): shell steps that only ever ran on a GitHub runner. As a
//! test it runs wherever `chore test` or `chore test:oracle` does, with
//! the same checks:
//!
//! - a 32 MiB single-group image, formatted with a label and a UUID,
//!   passes `fsck.ext4 -fnv`, and `tune2fs -l` reads both back;
//! - a 320 MiB image (three groups, a short final one) and a 640 MiB one
//!   (five whole groups — the smallest count reaching past groups 0 and 1
//!   into the sparse-super powers-of-3/5/7 rule) pass `fsck.ext4 -fnv`;
//! - both open through BACKUP superblocks (`-b 32768` is group 1, `-b
//!   98304` group 3 at 4 KiB blocks), so each backup's `s_block_group_nr`
//!   and recomputed checksum must be right or the open fails outright;
//! - `dumpe2fs` shows the short final group of the 320 MiB image ending at
//!   the device's last block (81919), not at a full group boundary.
//!
//! `-f` forces a full check and `-n` refuses every repair, so fsck reports
//! without touching the image and exits non-zero on any problem. The
//! e2fsprogs tools run in the harness VM; THIS CRATE'S OWN `mkfs.ext4`
//! runs on the host, because it is the thing under test rather than an
//! oracle.

use fs_ext4_test_support::oracle;
mod cli_support;

const MKFS: &str = "mkfs.ext4";

fn image(name: &str, size: u64) -> String {
    let path = fs_ext4_test_support::temp_path!("mkfs_bin_fsck_{}_{name}.img", std::process::id());
    let _ = std::fs::remove_file(&path);
    // Sparse: the runner pays for ~2 MiB of metadata per group, not the size.
    std::fs::File::create(&path)
        .and_then(|f| f.set_len(size))
        .unwrap_or_else(|e| panic!("size {path}: {e}"));
    path
}

/// THIS CRATE'S OWN `mkfs.ext4`, on the host: it is the thing under
/// test, not an oracle, and it is a Rust binary the host just built.
fn mkfs_bin(args: &[&str]) -> String {
    let out = cli_support::tool(MKFS)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run {MKFS}: {e}"));
    report(MKFS, args, out.status.code());
    assert!(
        out.status.success(),
        "{MKFS} {} failed ({:?}):\n{}{}",
        args.join(" "),
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// An e2fsprogs tool, in the harness VM, which must succeed.
fn succeeds(tool: &str, args: &[&str]) -> String {
    let out = oracle(tool).args(args).output();
    assert!(
        out.status.success(),
        "{tool} {} failed ({:?}):\n{}{}",
        args.join(" "),
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn report(program: &str, args: &[&str], code: Option<i32>) {
    println!("[oracle host] {program} {} -> {code:?}", args.join(" "));
}

/// `fsck.ext4` must examine the volume and find nothing. Its exit status
/// is not that verdict (#280): the report is read.
fn fsck(args: &[&str]) {
    oracle("fsck.ext4")
        .args(args)
        .judged()
        .clean(&format!("fsck.ext4 {}", args.join(" ")));
}

#[test]
fn single_group_image_passes_fsck_and_keeps_its_label_and_uuid() {
    let img = image("single", 32 * 1024 * 1024);
    mkfs_bin(&[
        "-L",
        "CITEST",
        "-U",
        "deadbeef-cafe-1234-5678-0123456789ab",
        &img,
    ]);
    fsck(&["-fnv", &img]);

    // tune2fs prints the on-disk superblock: the arguments reached the
    // bytes fsck just validated.
    let sb = succeeds("tune2fs", &["-l", &img]);
    let field = |name: &str| {
        sb.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
            .unwrap_or_else(|| panic!("tune2fs -l has no {name}:\n{sb}"))
    };
    assert_eq!(field("Filesystem volume name"), "CITEST");
    assert_eq!(
        field("Filesystem UUID").to_lowercase(),
        "deadbeef-cafe-1234-5678-0123456789ab"
    );
    let _ = std::fs::remove_file(&img);
}

#[test]
fn multi_group_images_pass_fsck_through_primary_and_backup_superblocks() {
    let mg3 = image("mg3", 320 * 1024 * 1024);
    let mg5 = image("mg5", 640 * 1024 * 1024);
    mkfs_bin(&["-L", "CIMULTI3", &mg3]);
    mkfs_bin(&["-L", "CIMULTI5", &mg5]);

    fsck(&["-fnv", &mg3]);
    fsck(&["-fnv", &mg5]);
    fsck(&["-fn", "-b", "32768", "-B", "4096", &mg3]);
    fsck(&["-fn", "-b", "32768", "-B", "4096", &mg5]);
    fsck(&["-fn", "-b", "98304", "-B", "4096", &mg5]);

    // The short final group ends at the last block of the device.
    let groups = oracle("dumpe2fs").arg(&mg3).judged().clean("dumpe2fs");
    let groups = String::from_utf8_lossy(&groups.stdout);
    let layout: Vec<&str> = groups.lines().filter(|l| l.starts_with("Group ")).collect();
    assert!(
        groups.contains("Group 2: (Blocks 65536-81919)"),
        "group 2 of the 320 MiB image does not end at block 81919:\n{}",
        layout.join("\n")
    );
    let _ = std::fs::remove_file(&mg3);
    let _ = std::fs::remove_file(&mg5);
}
