//! The salvage reads of the superblock copies, the group descriptor tables
//! and inodes, against `dumpe2fs`, `debugfs` and `e2fsck` on volumes
//! `mkfs.ext2`, `mkfs.ext3` and `mkfs.ext4` made.
//!
//! The layouts cover what decides where the copies are: every group
//! holding a backup (ext2 without `sparse_super`), the classic sparse rule
//! (ext3), `sparse_super2`'s two named backups, `meta_bg`'s per-meta-group
//! blocks, and 1 KiB and 4 KiB blocks. For each volume:
//!
//! - every superblock copy [`superblock_copies`] lists is sound, and sits
//!   in the block `dumpe2fs` names as the group's primary or backup
//!   superblock, and no group `dumpe2fs` gives a superblock is left out;
//! - every primary descriptor's block bitmap, inode bitmap and inode table
//!   are the ones `dumpe2fs` prints for its group, and every backup
//!   descriptor lies in a range `dumpe2fs` lists as group descriptors;
//! - the root inode's size and link count are the ones `debugfs` prints.
//!
//! Then the volume is damaged as a failing disk would damage it, and the
//! tools confirm the damage is real before the salvage reads are judged:
//! a zeroed primary superblock (e2fsck reports a bad magic number and
//! turns to a backup; `dumpe2fs -o superblock=` reads the backup this
//! crate falls back to), and a flipped
//! byte in a checksummed group descriptor (e2fsck reports it; this crate
//! keeps the descriptor and marks it failed, covered by its backup).
//!
//! The tools run in the harness VM; the test fails without it.

use fs_core::{BlockRead, FileDevice};
use fs_ext4::salvage::{
    descriptor_table_copies, find_superblock, group_descriptors, read_inode, structure_damage,
    superblock_copies, DamageKind, Integrity, Severity,
};
use fs_ext4_test_support::oracle;
use std::collections::BTreeMap;

/// What `dumpe2fs` says about one group.
#[derive(Default, Debug)]
struct Group {
    superblock: Option<u64>,
    descriptors: Vec<(u64, u64)>,
    block_bitmap: u64,
    inode_bitmap: u64,
    inode_table: u64,
}

fn first_number(text: &str) -> u64 {
    text.trim()
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

fn range(text: &str) -> (u64, u64) {
    let text = text.trim().trim_end_matches(',');
    match text.split_once('-') {
        Some((a, b)) => (first_number(a), first_number(b)),
        None => {
            let a = first_number(text);
            (a, a)
        }
    }
}

fn groups_of(text: &str) -> BTreeMap<u64, Group> {
    let mut groups = BTreeMap::new();
    let mut current = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Group ") {
            current = rest.split(':').next().and_then(|g| g.parse().ok());
            if let Some(g) = current {
                groups.insert(g, Group::default());
            }
            continue;
        }
        let Some(g) = current else { continue };
        let group: &mut Group = groups.get_mut(&g).unwrap();
        for part in line.split(", ") {
            let part = part.trim();
            if let Some(at) = part
                .strip_prefix("Primary superblock at ")
                .or_else(|| part.strip_prefix("Backup superblock at "))
            {
                group.superblock = Some(first_number(at));
            } else if let Some(r) = part
                .strip_prefix("Group descriptors at ")
                .or_else(|| part.strip_prefix("Group descriptor at "))
            {
                group.descriptors.push(range(r));
            } else if let Some(at) = part.strip_prefix("Block bitmap at ") {
                group.block_bitmap = first_number(at);
            } else if let Some(at) = part.strip_prefix("Inode bitmap at ") {
                group.inode_bitmap = first_number(at);
            } else if let Some(r) = part.strip_prefix("Inode table at ") {
                group.inode_table = range(r).0;
            }
        }
    }
    groups
}

fn dumpe2fs(args: &[&str], image: &str) -> String {
    let out = oracle("dumpe2fs")
        .args(args)
        .arg(image)
        .judged()
        .clean("dumpe2fs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A volume `mkfs.<kind>` makes, in this test's scratch space.
fn make(tag: &str, kind: &str, mib: u64, args: &[&str]) -> String {
    let image =
        fs_ext4_test_support::temp_path!("fs_ext4_salvage_{tag}_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(mib << 20))
        .unwrap();
    let out = oracle(&format!("mkfs.{kind}"))
        .args(["-q", "-F"])
        .args(args)
        .arg(&image)
        .output();
    assert!(
        out.status.success(),
        "[{tag}] mkfs.{kind}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    image
}

/// `Name:  value` from a `debugfs stat` report.
fn stat_field(report: &str, name: &str) -> u64 {
    report
        .split(['\n', ' ', '\t'])
        .collect::<Vec<_>>()
        .windows(2)
        .find(|w| w[0] == name)
        .map(|w| first_number(w[1]))
        .unwrap_or_else(|| panic!("debugfs stat printed no {name}:\n{report}"))
}

/// Every salvage read of a pristine volume agrees with the tools.
fn compare(tag: &str, image: &str) {
    let source = FileDevice::open(image).unwrap();
    let source: &dyn BlockRead = &source;
    let chosen = find_superblock(source).unwrap();
    assert_eq!(chosen.group, 0, "[{tag}] the primary is sound");
    let sb = chosen.superblock.unwrap();
    let block_size = u64::from(sb.block_size());
    let groups = groups_of(&dumpe2fs(&[], image));
    assert_eq!(groups.len() as u64, sb.block_group_count(), "[{tag}]");

    let copies = superblock_copies(source, &sb);
    for copy in &copies {
        assert!(copy.is_sound(), "[{tag}] {copy:?}");
        assert_eq!(
            groups[&copy.group].superblock,
            Some(copy.location.offset / block_size),
            "[{tag}] superblock copy in group {}",
            copy.group
        );
    }
    let with_super = groups.values().filter(|g| g.superblock.is_some()).count();
    assert_eq!(copies.len(), with_super, "[{tag}] superblock copies");

    let checksummed = sb.feature_ro_compat & 0x0410 != 0; // gdt_csum | metadata_csum
    for record in group_descriptors(source, &sb, 0) {
        let want = if checksummed {
            Integrity::Verified
        } else {
            Integrity::Unchecked
        };
        assert_eq!(record.integrity, want, "[{tag}] {record:?}");
        let d = record.descriptor.unwrap();
        let g = &groups[&record.group];
        assert_eq!(
            (d.block_bitmap, d.inode_bitmap, d.inode_table),
            (g.block_bitmap, g.inode_bitmap, g.inode_table),
            "[{tag}] group {}",
            record.group
        );
    }
    let ranges: Vec<(u64, u64)> = groups
        .values()
        .flat_map(|g| g.descriptors.iter().copied())
        .collect();
    let copies_of_tables = descriptor_table_copies(&sb);
    let mut backups = 0;
    for copy in 1..copies_of_tables {
        for record in group_descriptors(source, &sb, copy) {
            let block = record.location.offset / block_size;
            assert!(
                ranges.iter().any(|&(a, b)| (a..=b).contains(&block)),
                "[{tag}] backup {copy} of descriptor {} is in block {block}, which \
                 dumpe2fs lists as no descriptor block: {ranges:?}",
                record.group
            );
            assert!(record.descriptor.is_some(), "[{tag}] {record:?}");
            backups += 1;
        }
    }
    assert!(backups > 0, "[{tag}] no backup descriptors were read");

    let descriptors = group_descriptors(source, &sb, 0);
    let root = read_inode(source, &sb, &descriptors, 2).unwrap();
    assert_ne!(root.integrity, Integrity::Failed, "[{tag}] {root:?}");
    let root_inode = root.inode.unwrap();
    let out = oracle("debugfs")
        .args(["-R", "stat <2>"])
        .arg(image)
        .judged()
        .clean("debugfs stat");
    let report = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(root_inode.size, stat_field(&report, "Size:"), "[{tag}]");
    assert_eq!(
        u64::from(root_inode.links_count),
        stat_field(&report, "Links:"),
        "[{tag}]"
    );
    assert!(
        structure_damage(source).unwrap().is_empty(),
        "[{tag}] a fresh volume reports damage"
    );
}

/// The primary superblock zeroed: e2fsck cannot open the volume through
/// it, and this crate takes the geometry from the first backup -- the one
/// `dumpe2fs -o superblock=` reads, whose descriptor table agrees.
fn primary_superblock_destroyed(tag: &str, image: &str) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(image)
        .and_then(|f| std::os::unix::fs::FileExt::write_all_at(&f, &[0u8; 1024], 1024))
        .unwrap();
    let out = oracle("e2fsck")
        .args(["-fn"])
        .arg(image)
        .judged()
        .examined(&format!("{tag}: zeroed primary"));
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        said.contains("Bad magic number in super-block") && said.contains("backup"),
        "[{tag}] e2fsck did not report the primary unusable and turn to a backup:\n{said}"
    );
    let source = FileDevice::open(image).unwrap();
    let source: &dyn BlockRead = &source;
    let chosen = find_superblock(source).unwrap();
    assert_eq!(chosen.group, 1, "[{tag}]");
    let sb = chosen.superblock.unwrap();
    let block_size = sb.block_size();
    let backup_block = chosen.location.offset / u64::from(block_size);
    let groups = groups_of(&dumpe2fs(
        &[
            "-o",
            &format!("superblock={backup_block}"),
            "-o",
            &format!("blocksize={block_size}"),
        ],
        image,
    ));
    for record in group_descriptors(source, &sb, 1) {
        let d = record.descriptor.unwrap();
        let g = &groups[&record.group];
        assert_eq!(
            (d.block_bitmap, d.inode_bitmap, d.inode_table),
            (g.block_bitmap, g.inode_bitmap, g.inode_table),
            "[{tag}] group {} through the backup",
            record.group
        );
    }
    let damage = structure_damage(source).unwrap();
    assert!(
        damage.iter().any(|d| d.structure == "primary superblock"
            && d.kind == DamageKind::Signature
            && d.severity == Severity::Warning),
        "[{tag}] {damage:?}"
    );
}

/// A flipped byte in group 1's primary descriptor on a checksummed
/// volume: e2fsck reports a descriptor checksum, and this crate keeps the
/// descriptor, marks it failed and names the backup as covering it.
fn descriptor_checksum_broken(tag: &str, image: &str) {
    let source = FileDevice::open(image).unwrap();
    let sb = find_superblock(&source).unwrap().superblock.unwrap();
    let target = group_descriptors(&source, &sb, 0)[1].location.offset + 0x0C;
    drop(source);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .unwrap();
    let mut byte = [0u8];
    std::os::unix::fs::FileExt::read_exact_at(&file, &mut byte, target).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&file, &[byte[0] ^ 0xFF], target).unwrap();
    drop(file);
    let said = oracle("e2fsck")
        .args(["-fn"])
        .arg(image)
        .judged()
        .findings(&format!("{tag}: flipped descriptor"));
    assert!(
        said.to_lowercase().contains("checksum"),
        "[{tag}] e2fsck said nothing of a checksum:\n{said}"
    );
    let source = FileDevice::open(image).unwrap();
    let record = &group_descriptors(&source, &sb, 0)[1];
    assert_eq!(record.integrity, Integrity::Failed, "[{tag}] {record:?}");
    assert!(record.descriptor.is_some());
    let damage = structure_damage(&source).unwrap();
    assert!(
        damage.iter().any(|d| d.structure == "group descriptor 1"
            && d.kind == DamageKind::Checksum
            && d.severity == Severity::Warning),
        "[{tag}] {damage:?}"
    );
}

#[test]
fn ext2_volume_with_a_backup_in_every_group() {
    let image = make(
        "ext2",
        "ext2",
        24,
        &["-b", "1024", "-O", "^sparse_super,^resize_inode"],
    );
    compare("ext2", &image);
    primary_superblock_destroyed("ext2", &image);
    let _ = std::fs::remove_file(&image);
}

#[test]
fn ext3_volume_with_sparse_backups() {
    let image = make("ext3", "ext3", 64, &["-b", "1024"]);
    compare("ext3", &image);
    primary_superblock_destroyed("ext3", &image);
    let _ = std::fs::remove_file(&image);
}

#[test]
fn four_k_ext4_volume() {
    let image = make("ext4-4k", "ext4", 300, &["-b", "4096"]);
    compare("ext4-4k", &image);
    descriptor_checksum_broken("ext4-4k", &image);
    let _ = std::fs::remove_file(&image);
}

#[test]
fn sparse_super2_ext4_volume() {
    let image = make(
        "ext4-ss2",
        "ext4",
        64,
        &["-b", "1024", "-O", "sparse_super2"],
    );
    compare("ext4-ss2", &image);
    descriptor_checksum_broken("ext4-ss2", &image);
    let _ = std::fs::remove_file(&image);
}

#[test]
fn meta_bg_ext4_volume() {
    let image = make(
        "ext4-metabg",
        "ext4",
        64,
        &["-b", "1024", "-O", "meta_bg,^resize_inode"],
    );
    compare("ext4-metabg", &image);
    descriptor_checksum_broken("ext4-metabg", &image);
    let _ = std::fs::remove_file(&image);
}
