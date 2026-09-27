//! Format a fresh filesystem with the driver's own `mkfs::format_filesystem`
//! and require `e2fsck -fn` to pass on it
//! (`fs_ext4_test_support::assert_e2fsck_clean`; `RFE_KEEP_IMAGES=1` keeps the
//! images).
//!
//! `mkfs_roundtrip` and `mkfs_bin_smoke` already format + re-mount through the
//! driver's OWN reader, but that reader can't see a wrong checksum (the exact
//! blind spot that hid the Pi corruption). mkfs writes a large checksum surface
//! from scratch — superblock csum, the group-descriptor table, the group's
//! block/inode bitmap csums, the root inode csum, and the root dir-block tail —
//! none of which had ever faced an external checker (mkfs_bin_smoke's header
//! says so: "when one is wired up"). This is that checker.
//!
//! Two axes matter here, and for a long time only the first one was covered.
//!
//! **Block size.** The 1 KiB case uses the distinct first_data_block=1 layout
//! and a minimal journal; 2 KiB and 4 KiB use first_data_block=0.
//!
//! **Group count.** `format_block_groups` scales past one block group, and the
//! code that only runs at two-or-more groups — the backup superblock + GDT
//! loop, the `s_block_group_nr` patch and its checksum recompute, the
//! short-final-group bitmap padding, the cross-group free-block accumulation,
//! and the sparse-super powers-of-3/5/7 rule — is exactly the code every
//! real-world volume depends on and exactly the code no external checker had
//! ever seen. Every case below states the group count it produces, and asserts
//! it, so a geometry change cannot quietly collapse a multi-group case back to
//! a single group and take the coverage with it.
//!
//! One thing the default check does NOT cover: e2fsck opens the primary
//! superblock, so a backup whose `s_block_group_nr` or checksum is wrong goes
//! unnoticed until the day it is needed. `-b` makes it open a backup instead,
//! and at 4 KiB blocks group 1 starts at block 32768 and group 3 at 98304:
//!
//!   e2fsck -fn -b 32768 -B 4096 tmp/oracle/fs_ext4_mkfs_mg5_*.img
//!   e2fsck -fn -b 98304 -B 4096 tmp/oracle/fs_ext4_mkfs_mg5_*.img
//!
//! The `validate-mkfs-bin` CI job runs both forms against both multi-group
//! sizes on every push, so this is a gate rather than a recipe.
//!
//! All produced images carry metadata_csum + metadata_csum_seed.

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::fs::Filesystem;
use fs_ext4::mkfs;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const UUID: [u8; 16] = [
    0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x10, 0x32, 0x54, 0x76, 0x98, 0xBA, 0xDC, 0xFE,
];

/// Pre-size a tmp file, format it via the driver's mkfs, and return its path.
fn format_to_tmp(tag: &str, size: u64, block_size: u32) -> String {
    format_to_tmp_with_uuid(tag, size, block_size, Some(UUID))
}

fn format_to_tmp_with_uuid(
    tag: &str,
    size: u64,
    block_size: u32,
    uuid: Option<[u8; 16]>,
) -> String {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let path =
        fs_ext4_test_support::temp_path!("fs_ext4_mkfs_{tag}_{}_{n}.img", std::process::id());
    {
        let f = std::fs::File::create(&path)
            .unwrap_or_else(|e| panic!("create scratch image {path}: {e}"));
        f.set_len(size)
            .unwrap_or_else(|e| panic!("size scratch image {path}: {e}"));
    }
    {
        let dev = FileDevice::open_rw(&path).expect("open_rw");
        mkfs::format_filesystem(&dev, Some("MKFSORACLE"), uuid, size, block_size)
            .expect("format_filesystem");
        dev.flush().expect("flush");
    } // drop closes the file → bytes are on disk
    path
}

/// Mount the freshly-formatted image through the driver and sanity-check the
/// root, then leave it for the external e2fsck (or clean up).
///
/// `expect_groups` is the geometry contract, not a derived value: the caller
/// states how many block groups the size/block-size pair is supposed to lay
/// out, so a change to `blocks_per_group` or to the dispatch rule shows up as
/// a failing assertion rather than as a case that silently stops covering the
/// multi-group path.
fn check_and_done(path: &str, tag: &str, block_size: u32, expect_groups: usize) {
    {
        let dev = FileDevice::open(path).expect("ro");
        let fs = Filesystem::mount(Arc::new(dev)).expect("mount fresh fs");
        assert_eq!(fs.sb.block_size(), block_size, "[{tag}] block size");
        assert_eq!(fs.groups.len(), expect_groups, "[{tag}] block group count");
        assert!(fs.csum.enabled, "[{tag}] metadata_csum must be on");
        assert!(fs.sb.is_clean(), "[{tag}] fresh fs must be clean");
        let (root, _) = fs.read_inode_verified(2).expect("root inode verifies");
        assert!(root.is_dir(), "[{tag}] root must be a directory");
        assert_eq!(root.links_count, 2, "[{tag}] root links = 2");

        // Structural audit: the freshly-formatted block/inode bitmaps and the
        // stored free counters must already agree. This catches the
        // first_data_block=1 bitmap/count drift that e2fsck flags as "Free
        // blocks count wrong" on 1 KiB-block images, before e2fsck runs.
        let report = fs_ext4::fsck::audit(&fs, u32::MAX, u32::MAX).expect("audit");
        assert!(
            report.is_clean(),
            "[{tag}] fresh fs has structural anomalies: {:?}",
            report.anomalies
        );
    }
    fs_ext4_test_support::assert_e2fsck_clean(path, tag);
    if std::env::var_os("RFE_KEEP_IMAGES").is_some() {
        eprintln!("[{tag}] image: {path}");
    } else {
        let _ = fs::remove_file(path);
    }
}

#[test]
fn mkfs_4k_blocks_32m() {
    // 32 MiB / 4 KiB = 8192 blocks against a 32768-block group: one group.
    let p = format_to_tmp("4k", 32 * 1024 * 1024, 4096);
    check_and_done(&p, "4k", 4096, 1);
}

#[test]
fn mkfs_2k_blocks_16m() {
    // 16 MiB / 2 KiB = 8192 blocks against a 16384-block group: one group.
    let p = format_to_tmp("2k", 16 * 1024 * 1024, 2048);
    check_and_done(&p, "2k", 2048, 1);
}

#[test]
fn mkfs_1k_blocks_8m() {
    // 1 KiB blocks → first_data_block=1, minimal (1024-block) journal: a
    // distinct on-disk layout from the 4 KiB default. One group.
    let p = format_to_tmp("1k", 8 * 1024 * 1024, 1024);
    check_and_done(&p, "1k", 1024, 1);
}

#[test]
fn mkfs_4k_blocks_320m_short_final_group() {
    // 320 MiB / 4 KiB = 81920 blocks over 32768-block groups: three groups,
    // the last of which is HALF a group (16384 blocks). That short tail is
    // what makes this case worth its runtime — the formatter has to mark
    // bits `glen..blocks_per_group` as in-use so neither the kernel nor the
    // allocator can hand out blocks past the end of the device, and the
    // group's stored free count has to exclude them. Both are invisible to
    // this crate's own reader, which trusts the counters it is shown.
    //
    // Group 1 also carries the first superblock + GDT backup, so this is the
    // smallest case that runs the backup loop at all.
    let p = format_to_tmp("mg3", 320 * 1024 * 1024, 4096);
    check_and_done(&p, "mg3", 4096, 3);
}

#[test]
fn mkfs_4k_blocks_640m_sparse_super_backups() {
    // 640 MiB / 4 KiB = 163840 blocks: five whole groups, no short tail.
    // Five is the smallest group count that reaches past the "groups 0 and 1"
    // special case into the powers-of-3/5/7 rule, so backups land in groups
    // 0, 1 and 3 and NOT in 2 or 4. e2fsck reads those backups and checks
    // each one's `s_block_group_nr` and recomputed superblock checksum, which
    // is the only external check the backup path has ever had.
    let p = format_to_tmp("mg5", 640 * 1024 * 1024, 4096);
    check_and_done(&p, "mg5", 4096, 5);
}

#[test]
fn mkfs_with_a_generated_uuid() {
    // No caller UUID: mkfs draws one through runtime::fill_random, the path
    // the browser build takes too (#294). The volume must still be one
    // e2fsck accepts, and the UUID a version-4 one.
    let p = format_to_tmp_with_uuid("genuuid", 16 * 1024 * 1024, 4096, None);
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&p).expect("ro"))).expect("mount");
        let uuid = fs.sb.uuid;
        assert_ne!(uuid, [0u8; 16], "a generated UUID is not all zeros");
        assert_eq!(uuid[6] >> 4, 4, "version nibble of {uuid:02x?}");
        assert_eq!(uuid[8] >> 6, 0b10, "variant bits of {uuid:02x?}");
    }
    check_and_done(&p, "genuuid", 4096, 1);
}
