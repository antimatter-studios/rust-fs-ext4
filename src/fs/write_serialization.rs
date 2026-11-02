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

mod needs_host {
    use super::*;
    use crate::block_io::FileDevice;

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
