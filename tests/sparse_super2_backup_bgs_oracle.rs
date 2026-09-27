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
//! `e2fsck -fn` — once through the primary superblock and once through the
//! backup in the last group, which must still be intact. The tools run in
//! the harness VM.

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
fn filling_into_the_last_group_keeps_its_backup_superblock() {
    let path = make_image("fill");
    let backups = dumpe2fs_backup_bgs(&path);

    let (last, backup_block) = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&path).expect("open_rw")))
            .expect("mount");
        let last = fs.groups.len() as u64 - 1;
        assert!(
            backups.contains(&(last as u32)),
            "mke2fs should put a backup in the last group ({last}); it named {backups:?}"
        );
        let backup_block =
            u64::from(fs.sb.first_data_block) + last * u64::from(fs.sb.blocks_per_group);

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
        (last, backup_block)
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

    e2fsck_clean(&["-fn", &path], "primary superblock");
    let b = backup_block.to_string();
    e2fsck_clean(
        &["-fn", "-b", &b, "-B", "1024", &path],
        "backup superblock in the last group",
    );
    let _ = std::fs::remove_file(&path);
}
