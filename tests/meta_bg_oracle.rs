//! META_BG volumes mount, read as e2fsprogs reads them, and stay e2fsck-clean
//! through writes (#73).
//!
//! With `META_BG` the group descriptors are not one table after the
//! superblock: each meta group (`block_size / desc_size` groups) keeps its own
//! descriptor block at the head of its first group, with backups in its second
//! and last. `mke2fs` turns it on for large volumes. The images here are
//! small, with groups small enough that there are several meta groups:
//! 128 groups at 1 KiB (16 descriptors per block) and at 2 KiB (32).
//!
//! The reference is e2fsprogs: `mkfs.ext4 -d` writes known files, `dumpe2fs`
//! reports every group's free-block count, and `e2fsck -fn` judges the volume
//! after this crate writes to it. Fails when e2fsprogs is not installed
//! (they run in the harness VM).

#![cfg(unix)]

use fs_ext4::block_io::FileDevice;
use fs_ext4::Filesystem;
use std::collections::BTreeMap;
use std::sync::Arc;

fn run(tool: &str, args: &[&str]) -> (Option<i32>, String) {
    let out = fs_ext4_test_support::oracle(tool).args(args).output();
    (
        out.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// Deterministic file contents, so the test needs no randomness.
fn content(i: usize) -> Vec<u8> {
    (0..i * 3001)
        .map(|j| (j as u32).wrapping_mul(2654435761).rotate_left(i as u32) as u8)
        .collect()
}

fn read_file(fs: &Filesystem, path: &str) -> Vec<u8> {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    let ino = fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, path)
        .unwrap_or_else(|e| panic!("lookup {path}: {e:?}"));
    let (inode, _) = fs.read_inode_verified(ino).unwrap();
    fs_ext4::file_io::read_all(fs, &inode).unwrap()
}

/// `dumpe2fs` on `image`: it must describe the volume and find no group
/// descriptor whose checksum is wrong (`csum 0x1234 (EXPECTED 0xd2c4)`).
fn dumpe2fs(image: &str) -> String {
    let out = fs_ext4_test_support::oracle("dumpe2fs")
        .arg(image)
        .judged()
        .clean("dumpe2fs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Group number → free blocks, as `dumpe2fs` reports them.
fn dumpe2fs_free_blocks(image: &str) -> BTreeMap<usize, u64> {
    let log = dumpe2fs(image);
    let mut out = BTreeMap::new();
    let mut group = None;
    for line in log.lines() {
        if let Some(rest) = line.strip_prefix("Group ") {
            group = rest.split(':').next().and_then(|g| g.parse().ok());
        } else if let (Some(g), Some(free)) = (group, line.trim().split(" free blocks").next()) {
            if line.contains(" free blocks, ") {
                out.insert(g, free.trim().parse().unwrap());
                group = None;
            }
        }
    }
    out
}

/// Group number → blocks at its head that `dumpe2fs` names as the
/// superblock, its descriptor block(s) and reserved GDT blocks.
fn dumpe2fs_head_blocks(image: &str) -> BTreeMap<usize, u64> {
    let log = dumpe2fs(image);
    let span = |text: &str| -> u64 {
        let range = text.trim().trim_end_matches(',');
        match range.split_once('-') {
            Some((a, b)) => b.parse::<u64>().unwrap() - a.parse::<u64>().unwrap() + 1,
            None => 1,
        }
    };
    let mut out = BTreeMap::new();
    let mut group = None;
    for line in log.lines() {
        if let Some(rest) = line.strip_prefix("Group ") {
            group = rest.split(':').next().and_then(|g| g.parse().ok());
            if let Some(g) = group {
                out.insert(g, 0);
            }
            continue;
        }
        let Some(g) = group else { continue };
        for part in line.split(", ") {
            let part = part.trim();
            let n = if part.contains("superblock at") {
                1
            } else if let Some(r) = part
                .strip_prefix("Group descriptors at ")
                .or_else(|| part.strip_prefix("Group descriptor at "))
                .or_else(|| part.strip_prefix("Reserved GDT blocks at "))
            {
                span(r)
            } else {
                0
            };
            *out.get_mut(&g).unwrap() += n;
        }
    }
    out
}

fn meta_bg_volume(block_size: u32) {
    let mkfs = "mkfs.ext4";
    let debugfs = "debugfs";
    let tag = format!("meta_bg_{block_size}");
    let root = fs_ext4_test_support::temp_path!("fs_ext4_{tag}_{}", std::process::id());
    std::fs::create_dir_all(format!("{root}/sub")).unwrap();
    for i in 1..=40 {
        std::fs::write(format!("{root}/f{i}.bin"), content(i)).unwrap();
    }
    for i in 1..=30 {
        std::fs::write(format!("{root}/sub/s{i}"), format!("n{i}\n")).unwrap();
    }
    let image = format!("{root}.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(128 * 1024 * 1024))
        .unwrap();
    let blocks_per_group = if block_size == 1024 { 1024 } else { 512 };
    let (code, log) = run(
        mkfs,
        &[
            "-q",
            "-F",
            "-b",
            &block_size.to_string(),
            "-g",
            &blocks_per_group.to_string(),
            "-N",
            "4096",
            "-O",
            "meta_bg,^resize_inode",
            "-d",
            &root,
            &image,
        ],
    );
    assert_eq!(code, Some(0), "[{tag}] {log}");

    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap()))
            .unwrap_or_else(|e| panic!("[{tag}] mount: {e:?}"));
        let dpb = fs.sb.descs_per_block() as usize;
        assert!(
            fs.groups.len() >= 3 * dpb,
            "[{tag}] {} groups is not several meta groups of {dpb}",
            fs.groups.len()
        );
        for (g, free) in dumpe2fs_free_blocks(&image) {
            assert_eq!(
                u64::from(fs.groups[g].free_blocks_count),
                free,
                "[{tag}] group {g}'s descriptor"
            );
        }
        // What the allocator reserves at a group's head when it wakes a
        // BLOCK_UNINIT group -- the second and last group of each meta group
        // hold a descriptor backup there.
        for (g, head) in dumpe2fs_head_blocks(&image) {
            assert_eq!(
                fs.sb.group_head_metadata_blocks(g as u64),
                head,
                "[{tag}] group {g}'s superblock and descriptor blocks"
            );
        }
        for i in 1..=40 {
            assert!(
                read_file(&fs, &format!("/f{i}.bin")) == content(i),
                "[{tag}] /f{i}.bin"
            );
        }
        assert_eq!(read_file(&fs, "/sub/s30"), b"n30\n");
    }

    // Writes spread across the volume: enough data to allocate outside the
    // first meta group, and the directories and counters that go with it.
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap()))
            .unwrap_or_else(|e| panic!("[{tag}] mount rw: {e:?}"));
        fs.apply_mkdir("/w", 0o755).unwrap();
        for i in 0..12 {
            let path = format!("/w/big{i}");
            fs.apply_create(&path, 0o644).unwrap();
            fs.apply_replace_file_content(&path, &content(10 + i))
                .unwrap_or_else(|e| panic!("[{tag}] write {path}: {e:?}"));
        }
        fs.apply_unlink("/f3.bin").unwrap();
    }
    fs_ext4_test_support::assert_e2fsck_clean(&image, "e2fsck -fn");
    let (code, log) = run(debugfs, &["-R", "cat /w/big11", &image]);
    assert_eq!(code, Some(0), "{log}");
    let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
    for (g, free) in dumpe2fs_free_blocks(&image) {
        assert_eq!(
            u64::from(fs.groups[g].free_blocks_count),
            free,
            "[{tag}] group {g}'s descriptor after writes"
        );
    }
    assert!(read_file(&fs, "/w/big11") == content(21));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&image);
}

#[test]
fn a_1k_meta_bg_volume() {
    meta_bg_volume(1024);
}

#[test]
fn a_2k_meta_bg_volume() {
    meta_bg_volume(2048);
}
