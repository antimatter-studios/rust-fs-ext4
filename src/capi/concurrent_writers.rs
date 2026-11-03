use super::*;
use crate::features::FsFlavor;
use crate::fs::concurrent_writers::{formatted, interleave};
use crate::superblock::Superblock;

fn concurrent_creates(flavor: FsFlavor) {
    let dev = formatted(flavor, false);
    let fs = Filesystem::mount(dev.clone()).unwrap();
    fs.apply_create("/warm", 0o644).unwrap();
    fs.apply_create("/stable", 0o644).unwrap();
    fs.apply_replace_file_content("/stable", b"readers remain available")
        .unwrap();
    let before = Superblock::read(dev.as_ref()).unwrap().free_inodes_count;
    let handle = Box::new(fs_ext4_fs_t { fs });
    let address = (&*handle as *const fs_ext4_fs_t) as usize;
    let (first, second) = interleave(
        &handle.fs,
        move || unsafe { fs_ext4_create(address as *mut fs_ext4_fs_t, c"/a".as_ptr(), 0o644) },
        move || unsafe { fs_ext4_create(address as *mut fs_ext4_fs_t, c"/b".as_ptr(), 0o644) },
    );
    assert_ne!(first, 0);
    assert_ne!(second, 0);
    assert_ne!(
        first, second,
        "two C ABI writers must receive distinct inodes"
    );
    drop(handle);
    let reopened = Filesystem::mount(dev.clone()).unwrap();
    assert_eq!(reopened.lookup_path_bytes(b"/a").unwrap(), first);
    assert_eq!(reopened.lookup_path_bytes(b"/b").unwrap(), second);
    assert_eq!(reopened.sb.free_inodes_count, before - 2);
}

#[test]
fn concurrent_capi_creates_without_journal() {
    concurrent_creates(FsFlavor::Ext4);
}

#[test]
fn concurrent_capi_creates_with_journal() {
    concurrent_creates(FsFlavor::Ext3);
}

fn concurrent_create_and_large_file_growth(flavor: FsFlavor) {
    let dev = formatted(flavor, false);
    let fs = Filesystem::mount(dev.clone()).unwrap();
    fs.apply_create("/stable", 0o644).unwrap();
    fs.apply_replace_file_content("/stable", b"readers remain available")
        .unwrap();
    let target = fs.apply_create("/big", 0o644).unwrap();
    for index in 0..32 {
        fs.apply_create(&format!("/padding-{index}"), 0o644)
            .unwrap();
    }
    let before = Superblock::read(dev.as_ref()).unwrap().free_inodes_count;
    let handle = Box::new(fs_ext4_fs_t { fs });
    let address = (&*handle as *const fs_ext4_fs_t) as usize;
    let size = 3 * 1024 * 1024 * 1024;
    let (created, grown) = interleave(
        &handle.fs,
        move || unsafe { fs_ext4_create(address as *mut fs_ext4_fs_t, c"/a".as_ptr(), 0o644) },
        move || unsafe { fs_ext4_truncate(address as *mut fs_ext4_fs_t, c"/big".as_ptr(), size) },
    );
    assert_ne!(created, 0);
    assert_eq!(grown, 0);
    drop(handle);
    let reopened = Filesystem::mount(dev).unwrap();
    assert_eq!(reopened.lookup_path_bytes(b"/a").unwrap(), created);
    assert_eq!(reopened.stat_ino(target).unwrap().size, size);
    assert_eq!(reopened.sb.free_inodes_count, before - 1);
    assert_ne!(
        reopened.sb.feature_ro_compat & crate::features::RoCompat::LARGE_FILE.bits(),
        0
    );
}

#[test]
fn concurrent_capi_large_file_activation_without_journal() {
    concurrent_create_and_large_file_growth(FsFlavor::Ext4);
}

#[test]
fn concurrent_capi_large_file_activation_with_journal() {
    concurrent_create_and_large_file_growth(FsFlavor::Ext3);
}
