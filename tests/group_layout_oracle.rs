//! Where each group's descriptor lives, and how many blocks head each
//! group, against `dumpe2fs` on volumes `mkfs.ext4` made or the kernel grew.
//!
//! The volumes cover the layouts the documentation describes: one
//! descriptor table after the superblock (sparse, not sparse, and
//! SPARSE_SUPER2's two named backups) and META_BG's per-meta-group blocks
//! (64- and 32-byte descriptors, and a short last meta group), at 1 KiB
//! and 4 KiB blocks, each with dozens to hundreds of groups.
//!
//! `mkfs.ext4` leaves `s_first_meta_bg` at 0 on each META_BG volume above,
//! where the two ways of reading it (a meta group number, or a block group
//! number) agree. So the last two volumes are grown by the kernel instead:
//! made without reserved GDT blocks, then resized online in the guest past
//! what the descriptor table after the superblock holds. The kernel turns
//! META_BG on for that and records a nonzero `s_first_meta_bg` ("First meta
//! block group: 1" in `dumpe2fs`), the one case where the table after the
//! superblock and the meta groups' own blocks are both in use. This crate
//! then writes into those volumes' `BLOCK_UNINIT` groups, and `e2fsck -fn`
//! judges the result.
//!
//! For every group:
//! - the descriptor [`Superblock::descriptor_location`] names, read from
//!   the image, must give the block bitmap `dumpe2fs` reports for it, and
//!   its block must be the primary copy `dumpe2fs` lists (the table after
//!   the superblock, or the meta group's own block);
//! - [`Superblock::group_head_metadata_blocks`] must equal the superblock,
//!   descriptor and reserved GDT blocks `dumpe2fs` lists for the group.
//!
//! The tools run in the harness VM; the test fails without it.

use fs_ext4::bgd::BgdFlags;
use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use fs_ext4::superblock::Superblock;
use fs_ext4_test_support::{guest_kernel_write, oracle};
use std::collections::BTreeMap;
use std::sync::Arc;

/// What `dumpe2fs` says about one group.
#[derive(Default, Debug)]
struct Group {
    block_bitmap: u64,
    /// Blocks listed as superblock, group descriptor(s) or reserved GDT.
    head: u64,
    /// The first block of each "Group descriptor(s) at A-B" range.
    descriptor_ranges: Vec<(u64, u64)>,
}

fn range(text: &str) -> (u64, u64) {
    let text = text.trim().trim_end_matches(',');
    match text.split_once('-') {
        Some((a, b)) => (a.parse().unwrap(), b.parse().unwrap()),
        None => {
            let a = text.parse().unwrap();
            (a, a)
        }
    }
}

fn dumpe2fs_text(image: &str) -> String {
    let out = oracle("dumpe2fs").arg(image).judged().clean("dumpe2fs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The value of one `Name:   value` line of the `dumpe2fs` header.
fn header<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .map(str::trim)
}

fn dumpe2fs(image: &str) -> BTreeMap<u64, Group> {
    groups_of(&dumpe2fs_text(image))
}

fn groups_of(text: &str) -> BTreeMap<u64, Group> {
    let mut groups = BTreeMap::new();
    let mut current: Option<u64> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Group ") {
            current = rest.split(':').next().and_then(|g| g.parse().ok());
            if let Some(g) = current {
                groups.insert(g, Group::default());
            }
            continue;
        }
        let Some(g) = current else { continue };
        let group = groups.get_mut(&g).unwrap();
        for part in line.split(", ") {
            let part = part.trim();
            if part.contains("superblock at") {
                group.head += 1;
            } else if let Some(r) = part
                .strip_prefix("Group descriptors at ")
                .or_else(|| part.strip_prefix("Group descriptor at "))
            {
                let (a, b) = range(r);
                group.head += b - a + 1;
                group.descriptor_ranges.push((a, b));
            } else if let Some(r) = part.strip_prefix("Reserved GDT blocks at ") {
                let (a, b) = range(r);
                group.head += b - a + 1;
            } else if let Some(r) = part.strip_prefix("Block bitmap at ") {
                group.block_bitmap = r.split_whitespace().next().unwrap().parse().unwrap();
            }
        }
    }
    groups
}

fn check(tag: &str, block_size: u32, blocks_per_group: u32, mib: u64, features: &str) {
    let image = make(tag, block_size, blocks_per_group, mib, features);
    compare(tag, &image);
    let _ = std::fs::remove_file(&image);
}

/// A volume `mkfs.ext4` makes, in this test's scratch space.
fn make(tag: &str, block_size: u32, blocks_per_group: u32, mib: u64, features: &str) -> String {
    let image = fs_ext4_test_support::temp_path!("fs_ext4_layout_{tag}_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(mib << 20))
        .unwrap();
    let mut mkfs = oracle("mkfs.ext4").args([
        "-q",
        "-F",
        "-b",
        &block_size.to_string(),
        "-g",
        &blocks_per_group.to_string(),
    ]);
    if !features.is_empty() {
        mkfs = mkfs.args(["-O", features]);
    }
    let out = mkfs.arg(&image).output();
    assert!(
        out.status.success(),
        "[{tag}] mkfs.ext4: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    image
}

/// Every group's descriptor location and head size, against `dumpe2fs`.
fn compare(tag: &str, image: &str) {
    let groups = dumpe2fs(image);
    let bytes = fs_ext4_test_support::read_small_file(image).unwrap();
    let sb = Superblock::read(&FileDevice::open(image).unwrap()).unwrap();
    assert_eq!(groups.len() as u64, sb.block_group_count(), "[{tag}]");
    assert!(groups.len() >= 30, "[{tag}] only {} groups", groups.len());
    let bs = u64::from(sb.block_size());
    let per_block = sb.descs_per_block();

    for (&g, reported) in &groups {
        let (block, offset) = sb.descriptor_location(g);
        let at = (block * bs) as usize + offset;
        let lo = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let hi = if sb.is_64bit() && sb.desc_size >= 64 {
            u32::from_le_bytes(bytes[at + 0x20..at + 0x24].try_into().unwrap())
        } else {
            0
        };
        assert_eq!(
            u64::from(lo) | (u64::from(hi) << 32),
            reported.block_bitmap,
            "[{tag}] group {g}: the descriptor at block {block} + {offset}"
        );

        // The primary copy: in group 0's table, or in the head of the
        // first group of the meta group that lists this descriptor block.
        let holder = (g / per_block) * per_block;
        let in_group_zero = groups[&0]
            .descriptor_ranges
            .first()
            .is_some_and(|&(a, b)| (a..=b).contains(&block) && block - a == g / per_block);
        let in_holder = groups[&holder]
            .descriptor_ranges
            .first()
            .is_some_and(|&(a, b)| a == b && a == block);
        assert!(
            in_group_zero || in_holder,
            "[{tag}] group {g}: block {block} is not where dumpe2fs lists its primary \
             descriptor ({:?} in group 0, {:?} in group {holder})",
            groups[&0].descriptor_ranges,
            groups[&holder].descriptor_ranges
        );

        assert_eq!(
            sb.group_head_metadata_blocks(g),
            reported.head,
            "[{tag}] group {g}'s superblock, descriptor and reserved GDT blocks"
        );
    }
}

/// A volume the kernel converted to META_BG by growing it online, from
/// `start_mib` to `end_mib` at 1 KiB blocks and 1 MiB groups. mkfs.ext4 is
/// given no reserved GDT blocks, so the table after the superblock cannot
/// grow in place.
///
/// Then, for every group, [`compare`]. Then this crate writes a file
/// filling nearly all the free space, so its allocator hands out blocks in
/// `BLOCK_UNINIT` groups past `s_first_meta_bg` whose heads hold a meta
/// group's descriptor copy, and must skip exactly that head:
/// `e2fsck -fn` must pass and the comparison must hold afterwards too.
fn kernel_grown(tag: &str, features: &str, start_mib: u64, end_mib: u64) {
    let image = make(tag, 1024, 1024, start_mib, features);
    let script = format!(
        r#"dev="$(findmnt -no SOURCE "$MNT")"
back="$(losetup -nO BACK-FILE "$dev")"
truncate -s {end_mib}M "$back"
losetup -c "$dev"
resize2fs "$dev"
"#
    );
    let out = guest_kernel_write(&image, &script);
    assert!(
        out.status.success(),
        "[{tag}] online resize in the guest: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let text = dumpe2fs_text(&image);
    let features_line = header(&text, "Filesystem features").unwrap_or_default();
    assert!(
        features_line.split_whitespace().any(|f| f == "meta_bg"),
        "[{tag}] the kernel did not turn META_BG on: {features_line}"
    );
    let first_meta_bg: u64 = header(&text, "First meta block group")
        .unwrap_or_else(|| panic!("[{tag}] dumpe2fs names no first meta block group"))
        .parse()
        .unwrap();
    assert_ne!(first_meta_bg, 0, "[{tag}] this tests nothing");
    let sb = Superblock::read(&FileDevice::open(&image).unwrap()).unwrap();
    assert_eq!(sb.first_meta_bg(), first_meta_bg, "[{tag}]");
    compare(tag, &image);
    fs_ext4_test_support::assert_e2fsck_clean(&image, &format!("{tag}: as the kernel grew it"));

    // The groups past s_first_meta_bg that hold a descriptor copy and that
    // the kernel left BLOCK_UNINIT.
    let per_block = sb.descs_per_block();
    let holds_copy = |g: u64| {
        let place = g % per_block;
        g / per_block >= first_meta_bg && (place <= 1 || place == per_block - 1)
    };
    let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
    let uninit = |fs: &Filesystem| -> Vec<u64> {
        (0..fs.groups.len() as u64)
            .filter(|&g| holds_copy(g))
            .filter(|&g| {
                fs.groups[g as usize]
                    .flags()
                    .contains(BgdFlags::BLOCK_UNINIT)
            })
            .collect()
    };
    let before = uninit(&fs);
    assert!(
        !before.is_empty(),
        "[{tag}] the kernel left no BLOCK_UNINIT group holding a descriptor copy"
    );
    let free: u64 = fs
        .groups
        .iter()
        .map(|g| u64::from(g.free_blocks_count))
        .sum();
    let blocks = free * 9 / 10;
    fs.apply_create("/fill", 0o644).expect("create");
    let chunk = vec![0xA5u8; 1 << 20];
    let mut at = 0u64;
    while at < blocks * 1024 {
        let n = (blocks * 1024 - at).min(chunk.len() as u64) as usize;
        fs.apply_pwrite("/fill", at, &chunk[..n]).expect("write");
        at += n as u64;
    }
    drop(fs);

    let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
    let after = uninit(&fs);
    drop(fs);
    assert!(
        after.len() < before.len(),
        "[{tag}] nothing was written to a BLOCK_UNINIT group past s_first_meta_bg \
         holding a descriptor copy: {before:?} are all still uninit"
    );
    fs_ext4_test_support::assert_e2fsck_clean(&image, &format!("{tag}: after this crate wrote"));
    compare(tag, &image);
    let _ = std::fs::remove_file(&image);
}

#[test]
fn a_1k_sparse_volume_with_reserved_gdt_blocks() {
    check("1k_sparse", 1024, 1024, 64, "");
}

#[test]
fn a_1k_volume_without_sparse_super() {
    check("1k_nosparse", 1024, 1024, 32, "^sparse_super,^resize_inode");
}

#[test]
fn a_1k_volume_with_sparse_super2() {
    check("1k_sparse2", 1024, 1024, 32, "sparse_super2,^resize_inode");
}

#[test]
fn a_1k_meta_bg_volume() {
    check("1k_meta", 1024, 1024, 64, "meta_bg,^resize_inode");
}

#[test]
fn a_4k_volume_mkfs_makes_meta_bg() {
    // mkfs.ext4 turns META_BG on by itself here: 256 groups of 4 MiB.
    check("4k_flex", 4096, 1024, 1024, "");
}

#[test]
fn a_4k_meta_bg_volume_with_32_byte_descriptors_and_a_short_last_meta_group() {
    // 128 descriptors per block, 200 groups: the second meta group stops
    // at group 199, so its "last group" copy has nowhere to go.
    check("4k_meta32", 4096, 256, 200, "meta_bg,^resize_inode,^64bit");
}

#[test]
fn a_4k_volume_without_flex_bg() {
    check("4k_noflex", 4096, 4096, 512, "^flex_bg");
}

#[test]
fn a_volume_the_kernel_converted_to_meta_bg_by_online_resize() {
    // 64-byte descriptors, 16 to a block: the first 16 groups keep the
    // table after the superblock, and meta groups 1 to 4 have their own.
    kernel_grown("kernel_meta", "^resize_inode", 16, 80);
}

#[test]
fn a_volume_with_32_byte_descriptors_the_kernel_converted_to_meta_bg() {
    // 32 to a block: meta group 1 is groups 32 to 63, and the last meta
    // group (64 to 79) is short.
    kernel_grown("kernel_meta32", "^resize_inode,^64bit", 16, 80);
}
