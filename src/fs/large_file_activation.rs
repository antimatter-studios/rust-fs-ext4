use super::concurrent_writers::formatted;
use super::*;
use crate::features::FsFlavor;

fn has_large_file(dev: &dyn BlockDevice) -> bool {
    Superblock::read(dev).unwrap().feature_ro_compat & features::RoCompat::LARGE_FILE.bits() != 0
}

#[test]
fn truncate_activates_large_file_at_two_gib_and_metadata_writes_keep_it() {
    for flavor in [FsFlavor::Ext2, FsFlavor::Ext3, FsFlavor::Ext4] {
        let dev = formatted(flavor, false);
        let fs = Filesystem::mount(dev.clone()).unwrap();
        let ino = fs.apply_create("/file", 0o644).unwrap();
        fs.apply_truncate_grow(ino, i32::MAX as u64).unwrap();
        assert!(!has_large_file(dev.as_ref()), "{flavor:?}");
        fs.apply_truncate_grow(ino, i32::MAX as u64 + 1).unwrap();
        assert!(has_large_file(dev.as_ref()), "{flavor:?}");
        fs.apply_chmod_ino(ino, 0o600).unwrap();
        fs.apply_create("/later", 0o644).unwrap();
        assert!(has_large_file(dev.as_ref()), "{flavor:?}");
    }
}

#[test]
fn sparse_pwrite_activates_large_file_through_path_and_inode_entries() {
    for by_inode in [false, true] {
        let dev = formatted(FsFlavor::Ext4, false);
        let fs = Filesystem::mount(dev.clone()).unwrap();
        let ino = fs.apply_create("/file", 0o644).unwrap();
        let offset = i32::MAX as u64;
        let size = if by_inode {
            fs.apply_pwrite_ino(ino, offset, b"x").unwrap()
        } else {
            fs.apply_pwrite("/file", offset, b"x").unwrap()
        };
        assert_eq!(size, offset + 1);
        assert!(has_large_file(dev.as_ref()), "by_inode={by_inode}");
        let mut bytes = [0; 1];
        assert_eq!(fs.read_ino(ino, offset, &mut bytes).unwrap(), 1);
        assert_eq!(&bytes, b"x");
        drop(fs);
        let reopened = Filesystem::mount(dev).unwrap();
        assert_eq!(reopened.stat_ino(ino).unwrap().size, offset + 1);
    }
}

#[test]
fn a_volume_already_carrying_large_file_does_not_stage_the_superblock_for_it() {
    let dev = formatted(FsFlavor::Ext4, true);
    let fs = Filesystem::mount(dev.clone()).unwrap();
    let ino = fs.apply_create("/file", 0o644).unwrap();
    fs.apply_truncate_ino(ino, 3 * 1024 * 1024 * 1024).unwrap();
    let (_, raw) = fs.read_inode_verified(ino).unwrap();
    let mut buf = BlockBuffer::new(fs.sb.block_size());
    fs.buffer_write_inode(&mut buf, ino, &raw).unwrap();
    let superblock_block = crate::superblock::SUPERBLOCK_OFFSET / u64::from(fs.sb.block_size());
    assert!(!buf.dirty.contains_key(&superblock_block));
}

#[test]
fn shrinking_a_large_extent_file_does_not_clear_large_file() {
    let dev = formatted(FsFlavor::Ext4, false);
    let fs = Filesystem::mount(dev.clone()).unwrap();
    let ino = fs.apply_create("/file", 0o644).unwrap();
    fs.apply_truncate_ino(ino, 3 * 1024 * 1024 * 1024).unwrap();
    fs.apply_truncate_ino(ino, 0).unwrap();
    assert!(has_large_file(dev.as_ref()));
}
