//! `s_backup_bgs` is read from where `mke2fs` writes it.
//!
//! With `sparse_super2`, the only groups carrying a backup superblock are
//! the two named by `s_backup_bgs`, at superblock offset 0x24C. Reading it
//! from anywhere else yields zeros, and then no group but 0 is believed to
//! carry a backup: an uninit group's implied bitmap leaves out its backup
//! superblock and GDT, and file data is written over them.
//!
//! Both halves are judged by e2fsprogs, not by this crate: the parsed field
//! against `dumpe2fs -h`, and a volume filled into its last group against
//! `e2fsck -fn`, then each backup read back by `dumpe2fs -o superblock=`.
//! A backup's free counts and bitmap checksums are stale by design — they
//! are what mke2fs wrote — so a backup is judged by what never changes: the
//! superblock's identity and the group descriptors' block locations, which
//! must match the primary's. File data written over a backup changes both.
//! The tools run in the harness VM.

use fs_ext4::block_io::FileDevice;
use fs_ext4::Filesystem;
use std::sync::Arc;

const BLOCK: u64 = 1024;
const IMAGE_BYTES: u64 = 32 * 1024 * 1024;

fn make_image(tag: &str) -> String {
    let path =
        fs_ext4_test_support::temp_path!("fs_ext4_sparse_super2_{tag}_{}.img", std::process::id());
    std::fs::File::create(&path)
        .and_then(|f| f.set_len(IMAGE_BYTES))
        .expect("size the image");
    let out = fs_ext4_test_support::oracle("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-b",
            "1024",
            "-O",
            "sparse_super2,metadata_csum",
        ])
        .arg(&path)
        .output();
    assert!(
        out.status.success(),
        "mkfs.ext4 -O sparse_super2: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

/// The groups `dumpe2fs -h` names on its "Backup block groups:" line.
fn dumpe2fs_backup_bgs(path: &str) -> Vec<u32> {
    let out = fs_ext4_test_support::oracle("dumpe2fs")
        .args(["-h", path])
        .output();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "dumpe2fs -h: {}{}",
        text,
        String::from_utf8_lossy(&out.stderr)
    );
    let line = text
        .lines()
        .find(|l| l.starts_with("Backup block groups:"))
        .unwrap_or_else(|| panic!("dumpe2fs -h printed no \"Backup block groups:\" line:\n{text}"));
    line["Backup block groups:".len()..]
        .split_whitespace()
        .map(|n| n.parse().expect("a group number"))
        .collect()
}

/// What a backup superblock and its GDT share with the primary for the
/// life of the filesystem: the identity lines and every group's metadata
/// locations, with the (legitimately stale) bitmap checksums cut off.
fn fixed_layout(path: &str, superblock: Option<u64>) -> Vec<String> {
    let mut oracle = fs_ext4_test_support::oracle("dumpe2fs");
    if let Some(block) = superblock {
        oracle = oracle.args([
            "-o".to_string(),
            format!("superblock={block}"),
            "-o".to_string(),
            format!("blocksize={BLOCK}"),
        ]);
    }
    // Exit status is not the verdict: reading the current bitmaps through a
    // backup's stale checksums can fail after the descriptors are printed.
    let out = oracle.arg(path).output();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| {
            [
                "Filesystem magic number:",
                "Filesystem UUID:",
                "Block count:",
                "Blocks per group:",
                "Block bitmap at",
                "Inode bitmap at",
                "Inode table at",
            ]
            .iter()
            .any(|p| l.starts_with(p))
        })
        .map(|l| l.split(", csum").next().unwrap_or(l).to_string())
        .collect()
}

fn e2fsck_clean(args: &[&str], what: &str) {
    let out = fs_ext4_test_support::oracle("e2fsck").args(args).output();
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // `e2fsck -n` can print a problem as IGNORED and still exit 0.
    assert!(
        out.status.success() && !report.contains("IGNORED"),
        "{what}: e2fsck {args:?} found problems:\n{report}"
    );
}

#[test]
fn backup_bgs_matches_dumpe2fs() {
    let path = make_image("parse");
    let expected = dumpe2fs_backup_bgs(&path);
    assert!(
        !expected.is_empty(),
        "mke2fs named no backup groups, so this compares nothing"
    );

    let fs = Filesystem::mount(Arc::new(FileDevice::open(&path).expect("open"))).expect("mount");
    let parsed: Vec<u32> = fs
        .sb
        .backup_bgs
        .iter()
        .copied()
        .filter(|&g| g != 0)
        .collect();
    assert_eq!(
        parsed, expected,
        "s_backup_bgs as parsed disagrees with dumpe2fs -h"
    );
    for g in 0..fs.groups.len() as u64 {
        assert_eq!(
            fs.sb.group_has_super(g),
            g == 0 || expected.contains(&(g as u32)),
            "group_has_super({g}) disagrees with dumpe2fs's backup groups {expected:?}"
        );
    }
    drop(fs);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn filling_into_the_last_group_keeps_its_backup_superblocks() {
    let path = make_image("fill");
    let backups = dumpe2fs_backup_bgs(&path);

    let (last, first_data_block, blocks_per_group) = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&path).expect("open_rw")))
            .expect("mount");
        let last = fs.groups.len() as u64 - 1;
        assert!(
            backups.contains(&(last as u32)),
            "mke2fs should put a backup in the last group ({last}); it named {backups:?}"
        );

        // Nine tenths of the free space: enough that allocation has to wake
        // every group, including the last, without chasing ENOSPC.
        let target = fs.sb.free_blocks_count * BLOCK * 9 / 10;
        let chunk = vec![0xA5u8; 256 * 1024];
        let mut written = 0u64;
        let mut n = 0;
        while written < target {
            let file = format!("/f{n}");
            fs.apply_create(&file, 0o644).expect("create");
            fs.apply_pwrite(&file, 0, &chunk).expect("pwrite");
            written += chunk.len() as u64;
            n += 1;
        }
        fs.dev.flush().expect("flush");
        (
            last,
            u64::from(fs.sb.first_data_block),
            u64::from(fs.sb.blocks_per_group),
        )
    };

    {
        let fs =
            Filesystem::mount(Arc::new(FileDevice::open(&path).expect("open"))).expect("remount");
        assert!(
            !fs.groups[last as usize]
                .flags()
                .contains(fs_ext4::bgd::BgdFlags::BLOCK_UNINIT),
            "the fill never reached the last group, so it tested nothing"
        );
    }

    e2fsck_clean(&["-fn", &path], "after filling into the last group");

    let primary = fixed_layout(&path, None);
    let groups = last as usize + 1;
    assert_eq!(
        primary.len(),
        4 + 3 * groups,
        "dumpe2fs printed an unexpected layout for the primary:\n{}",
        primary.join("\n")
    );
    for &g in &backups {
        let block = first_data_block + u64::from(g) * blocks_per_group;
        assert_eq!(
            fixed_layout(&path, Some(block)),
            primary,
            "the backup superblock/GDT in group {g} (block {block}) no longer matches the primary"
        );
    }
    let _ = std::fs::remove_file(&path);
}
