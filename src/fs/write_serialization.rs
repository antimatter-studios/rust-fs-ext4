use super::concurrent_writers::{formatted, interleave, Event};
use super::*;
use crate::features::FsFlavor;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn a_panicking_writer_poison_stops_writes_and_keeps_the_volume_unclean() {
    for flavor in [FsFlavor::Ext4, FsFlavor::Ext3] {
        let dev = formatted(flavor, false);
        let fs = Filesystem::mount(dev.clone()).unwrap();
        *fs.write_test_hook.lock().unwrap() = Some(Arc::new(|event| {
            if event == Event::BeforeCommit {
                panic!("writer panicked after staging");
            }
        }));
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fs.apply_create("/interrupted", 0o644)
        }));
        assert!(failure.is_err());
        assert!(matches!(
            fs.apply_create("/later", 0o644),
            Err(Error::Corrupt(_))
        ));
        assert!(fs.stat_ino(2).unwrap().is_dir());
        assert!(fs.flush().is_err());
        drop(fs);
        assert!(!Superblock::read(dev.as_ref()).unwrap().is_clean());
    }
}

#[test]
fn a_write_lock_does_not_serialize_unrelated_mounts() {
    let first = Filesystem::mount(formatted(FsFlavor::Ext4, false)).unwrap();
    let second = Filesystem::mount(formatted(FsFlavor::Ext3, false)).unwrap();
    let held = first.lock_write().unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            let ino = second.apply_create("/independent", 0o644).unwrap();
            done_tx.send(ino).unwrap();
        });
        let result = done_rx.recv_timeout(Duration::from_secs(20));
        drop(held);
        assert!(result.unwrap() > 0);
        writer.join().unwrap();
    });
}

/// Put `ino` on the orphan chain the way an unlink of an open file does: its
/// name gone, no links, and the superblock's chain head naming it.
fn plant_unlinked_orphan(fs: &mut Filesystem, name: &[u8], ino: u32) {
    let (root, _) = fs.read_inode_verified(2).unwrap();
    let (inode, mut raw) = fs.read_inode_verified(ino).unwrap();
    raw[0x1A..0x1C].copy_from_slice(&0u16.to_le_bytes());
    raw[0x14..0x18].copy_from_slice(&0u32.to_le_bytes());
    fs.finalize_inode_raw(ino, inode.generation, &mut raw)
        .unwrap();
    let mut buf = BlockBuffer::new(fs.sb.block_size());
    fs.buffer_remove_dir_entry(&mut buf, 2, &root, name)
        .unwrap();
    fs.buffer_write_inode(&mut buf, ino, &raw).unwrap();
    fs.buffer_patch_sb_last_orphan(&mut buf, ino).unwrap();
    fs.commit_block_buffer(buf).unwrap();
    fs.refresh_metadata().unwrap();
}

/// Writer A staged a create, so its copy of the superblock still names the
/// orphan. Recovery then clears the chain head, and A's commit must not bring
/// it back.
fn create_during_orphan_recovery(flavor: FsFlavor) -> Arc<dyn BlockDevice> {
    let dev = formatted(flavor, false);
    let mut fs = Filesystem::mount(dev.clone()).unwrap();
    fs.apply_create("/warm", 0o644).unwrap();
    fs.apply_create("/stable", 0o644).unwrap();
    fs.apply_replace_file_content("/stable", b"readers remain available")
        .unwrap();
    let orphan = fs.apply_create("/orphan", 0o644).unwrap();
    plant_unlinked_orphan(&mut fs, b"orphan", orphan);
    let planted = Superblock::read(dev.as_ref()).unwrap();
    assert_eq!(planted.last_orphan, orphan);

    let (created, recovered) = interleave(
        &fs,
        || fs.apply_create("/a", 0o644),
        || fs.recover_orphans(),
    );
    let created = created.unwrap();
    assert_eq!(recovered.unwrap(), 1);
    drop(fs);

    let after = Superblock::read(dev.as_ref()).unwrap();
    assert_eq!(after.last_orphan, 0, "a stale superblock restored the head");
    assert_eq!(
        after.free_inodes_count, planted.free_inodes_count,
        "one create and one reclaimed orphan must cancel out"
    );
    let reopened = Filesystem::mount(dev.clone()).unwrap();
    assert_eq!(reopened.lookup_path_bytes(b"/a").unwrap(), created);
    assert!(reopened.lookup_path_bytes(b"/orphan").is_err());
    drop(reopened);
    dev
}

#[test]
fn a_concurrent_create_does_not_restore_the_orphan_head_without_journal() {
    create_during_orphan_recovery(FsFlavor::Ext4);
}

#[test]
fn a_concurrent_create_does_not_restore_the_orphan_head_with_journal() {
    create_during_orphan_recovery(FsFlavor::Ext3);
}

mod needs_host {
    use super::*;
    use crate::block_io::FileDevice;

    #[test]
    fn create_during_orphan_recovery_is_clean_to_e2fsck() {
        for flavor in [FsFlavor::Ext4, FsFlavor::Ext3] {
            let dev = create_during_orphan_recovery(flavor);
            let mut bytes = vec![0; dev.size_bytes() as usize];
            dev.read_at(0, &mut bytes).unwrap();
            let tag = format!("concurrent-orphan-head-{flavor:?}");
            let path = fs_ext4_test_support::temp_path!("{tag}.img");
            std::fs::write(&path, &bytes).unwrap();
            fs_ext4_test_support::assert_e2fsck_clean(&path, &tag);
        }
    }

    fn journaled_ext4_cases(tag: &str) -> Vec<String> {
        let fixture = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), "ext4-basic.img");
        let mut images = Vec::new();
        for case in ["creates", "xattr", "shared-inode", "large-file"] {
            let image = fs_ext4_test_support::temp_path!("concurrent-ext4-{tag}-{case}.img");
            std::fs::copy(&fixture, &image).unwrap();
            let dev = Arc::new(FileDevice::open_rw(&image).unwrap());
            let sb = Superblock::read(dev.as_ref()).unwrap();
            let mut raw = sb.raw.clone();
            let compat = if case == "xattr" {
                sb.feature_compat & !features::Compat::EXT_ATTR.bits()
            } else {
                sb.feature_compat
            };
            raw[0x5c..0x60].copy_from_slice(&compat.to_le_bytes());
            if case == "large-file" {
                let flags = sb.feature_ro_compat & !features::RoCompat::LARGE_FILE.bits();
                raw[0x64..0x68].copy_from_slice(&flags.to_le_bytes());
            }
            if Checksummer::from_superblock(&sb).enabled {
                let checksum = crate::checksum::linux_crc32c(!0, &raw[..0x3fc]);
                raw[0x3fc..0x400].copy_from_slice(&checksum.to_le_bytes());
            }
            dev.write_at(crate::superblock::SUPERBLOCK_OFFSET, &raw)
                .unwrap();
            let fs = Filesystem::mount(dev.clone()).unwrap();
            assert!(fs.journal.is_some());
            assert!(fs.sb.inode_size >= 256);
            fs.apply_create("/warm", 0o644).unwrap();
            fs.apply_create("/stable", 0o644).unwrap();
            fs.apply_replace_file_content("/stable", b"readers remain available")
                .unwrap();
            let target = fs.apply_create("/target", 0o644).unwrap();
            if case == "xattr" || case == "large-file" {
                for index in 0..32 {
                    fs.apply_create(&format!("/padding-{index}"), 0o644)
                        .unwrap();
                }
            }
            let before = Superblock::read(dev.as_ref()).unwrap().free_inodes_count;
            let (first, second) = interleave(
                &fs,
                || fs.apply_create("/a", 0o644),
                || match case {
                    "creates" => fs.apply_create("/b", 0o644),
                    "xattr" => fs
                        .apply_setxattr("/target", "user.concurrent", b"kept")
                        .map(|()| target),
                    "shared-inode" => fs.apply_truncate_ino(target, 65536).map(|()| target),
                    "large-file" => fs
                        .apply_truncate_ino(target, 3 * 1024 * 1024 * 1024)
                        .map(|()| target),
                    _ => unreachable!(),
                },
            );
            let (first, second) = (first.unwrap(), second.unwrap());
            drop(fs);
            let reopened = Filesystem::mount(dev.clone()).unwrap();
            assert_eq!(reopened.lookup_path_bytes(b"/a").unwrap(), first);
            assert_eq!(
                reopened.sb.free_inodes_count,
                before - if case == "creates" { 2 } else { 1 }
            );
            if case == "creates" {
                assert_ne!(first, second);
                assert_eq!(reopened.lookup_path_bytes(b"/b").unwrap(), second);
            } else if case == "xattr" {
                assert_ne!(
                    reopened.sb.feature_compat & features::Compat::EXT_ATTR.bits(),
                    0
                );
                let (inode, raw) = reopened.read_inode_verified(target).unwrap();
                assert_eq!(inode.file_acl, 0);
                let entries = crate::xattr::read_all(
                    reopened.dev.as_ref(),
                    &inode,
                    &raw,
                    reopened.sb.inode_size,
                    reopened.sb.block_size(),
                )
                .unwrap();
                assert!(entries
                    .iter()
                    .any(|entry| entry.name == "user.concurrent" && entry.value == b"kept"));
            } else {
                let same_block = bgd::locate_inode(&reopened.sb, &reopened.groups, first)
                    .unwrap()
                    .0
                    == bgd::locate_inode(&reopened.sb, &reopened.groups, target)
                        .unwrap()
                        .0;
                assert_eq!(same_block, case == "shared-inode");
                let expected = if case == "large-file" {
                    3 * 1024 * 1024 * 1024
                } else {
                    65536
                };
                assert_eq!(reopened.stat_ino(target).unwrap().size, expected);
                if case == "large-file" {
                    assert_ne!(
                        reopened.sb.feature_ro_compat & features::RoCompat::LARGE_FILE.bits(),
                        0
                    );
                }
            }
            drop(reopened);
            images.push(image);
        }
        images
    }

    #[test]
    fn journaled_ext4_preserves_both_writers() {
        assert_eq!(journaled_ext4_cases("readback").len(), 4);
    }

    #[test]
    fn journaled_ext4_writes_are_clean_to_e2fsck() {
        for image in journaled_ext4_cases("oracle") {
            fs_ext4_test_support::assert_e2fsck_clean(&image, "concurrent journaled ext4");
        }
    }
}
