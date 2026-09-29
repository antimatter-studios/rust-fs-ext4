//! `mkfs.ext4` as the multi-call binary runs it, judged by e2fsprogs in
//! the harness VM: the image passes `e2fsck -fn`, and `dumpe2fs -h`
//! agrees with the JSON report on label, UUID, block size, block count
//! and free blocks — so the report is what the disk says, not what the
//! tool believes it wrote.

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::{assert_e2fsck_clean, oracle};

fn dumpe2fs_field(header: &str, name: &str) -> String {
    header
        .lines()
        .find(|l| l.starts_with(&format!("{name}:")))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_else(|| panic!("dumpe2fs -h has no {name}:\n{header}"))
}

#[test]
fn the_image_mkfs_makes_is_clean_and_dumpe2fs_agrees_with_its_report() {
    // 1 KiB blocks at 8 MiB: one group, the only layout the formatter
    // makes at that block size (multi-group needs block_size >= 2048).
    for (size, block) in [("64M", "4096"), ("8M", "1024")] {
        let img = image_path(&format!("oracle-{size}"));
        let out =
            ok(tool("mkfs.ext4").args(["--size", size, "--label", "CLITEST", "-b", block, &img]));
        let report = stdout(&out);
        assert_e2fsck_clean(&img, &format!("mkfs.ext4 --size {size} -b {block}"));
        let header = oracle("dumpe2fs").args(["-h", &img]).output();
        let header = String::from_utf8_lossy(&header.stdout).into_owned();
        assert_eq!(dumpe2fs_field(&header, "Filesystem volume name"), "CLITEST");
        assert_eq!(
            dumpe2fs_field(&header, "Filesystem volume name"),
            json_field(&report, "label")
        );
        assert_eq!(
            dumpe2fs_field(&header, "Filesystem UUID"),
            json_field(&report, "uuid")
        );
        assert_eq!(
            dumpe2fs_field(&header, "Block size"),
            json_field(&report, "block_size")
        );
        assert_eq!(
            dumpe2fs_field(&header, "Block count"),
            json_field(&report, "total_blocks")
        );
        assert_eq!(
            dumpe2fs_field(&header, "Free blocks"),
            json_field(&report, "free_blocks")
        );
        assert_eq!(
            dumpe2fs_field(&header, "Inode count"),
            json_field(&report, "total_inodes")
        );
        assert_eq!(
            dumpe2fs_field(&header, "Free inodes"),
            json_field(&report, "free_inodes")
        );
        let _ = std::fs::remove_file(&img);
    }
}
