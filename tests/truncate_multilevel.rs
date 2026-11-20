//! Real ext4 trees must support the same shrink/unlink lifecycle as inline trees.
use fs_ext4::{
    block_io::{BlockDevice, FileDevice},
    extent::ExtentHeader,
    Filesystem,
};
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::Arc,
};

/// A scratch copy of the deep-extents fixture. A missing fixture fails the
/// test, naming the task that builds it; it never skips.
#[track_caller]
fn scratch(label: &str) -> PathBuf {
    let source = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), "ext4-deep-extents.img");
    let path = PathBuf::from(fs_ext4_test_support::temp_path!(
        "ext4-deep-{label}-{}.img",
        std::process::id()
    ));
    std::fs::copy(source, &path).unwrap();
    path
}
fn inode(fs: &Filesystem, name: &str) -> u32 {
    let mut read = |ino| fs.read_inode_verified(ino).map(|(inode, _)| inode);
    fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut read, name).unwrap()
}
/// `e2fsck -fn` must accept the image. It runs in the harness VM, and a
/// test that cannot reach it fails.
#[track_caller]
fn oracle(path: &std::path::Path) {
    fs_ext4_test_support::assert_e2fsck_clean(path.to_str().unwrap(), "truncate_multilevel");
}
/// `image` with the superblock's `s_state` and checksum zeroed. The first
/// write call on a mount marks the volume not clean (and a drop puts it
/// back), so a refused or no-op call is allowed to have touched those
/// bytes and nothing else.
fn outside_state(image: &std::path::Path) -> PathBuf {
    let snapshot = fs_ext4_test_support::snapshot_file(image).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&snapshot)
        .unwrap();
    file.seek(SeekFrom::Start(1024 + 0x3A)).unwrap();
    file.write_all(&[0; 2]).unwrap();
    file.seek(SeekFrom::Start(1024 + 0x3FC)).unwrap();
    file.write_all(&[0; 4]).unwrap();
    snapshot
}
#[test]
fn equal_size_deep_truncate_leaves_image_unchanged() {
    let path = scratch("equal");
    let fs = Filesystem::mount(Arc::new(
        FileDevice::open_rw(path.to_str().unwrap()).unwrap(),
    ))
    .unwrap();
    let ino = inode(&fs, "/sparse.bin");
    let (before, _) = fs.read_inode_verified(ino).unwrap();
    assert!(ExtentHeader::parse(&before.block).unwrap().depth > 0);
    let bytes = fs_ext4_test_support::snapshot_file(&path).unwrap();
    let outside_before = outside_state(&path);
    fs.apply_truncate_shrink(ino, before.size).unwrap();
    let outside_after = outside_state(&path);
    fs_ext4_test_support::assert_files_equal(&outside_after, &outside_before);
    drop(fs);
    fs_ext4_test_support::assert_files_equal(&path, &bytes);
    oracle(&path);
    std::fs::remove_file(path).unwrap();
}
#[test]
fn deep_shrink_and_unlink_free_tree_blocks() {
    let path = scratch("shrink");
    let fs = Filesystem::mount(Arc::new(
        FileDevice::open_rw(path.to_str().unwrap()).unwrap(),
    ))
    .unwrap();
    let ino = inode(&fs, "/sparse.bin");
    fs.apply_truncate_shrink(ino, 65_537).unwrap();
    let (after, _) = fs.read_inode_verified(ino).unwrap();
    assert_eq!(after.size, 65_537);
    drop(fs);
    oracle(&path);
    let fs = Filesystem::mount(Arc::new(
        FileDevice::open_rw(path.to_str().unwrap()).unwrap(),
    ))
    .unwrap();
    fs.apply_unlink("/sparse.bin").unwrap();
    drop(fs);
    oracle(&path);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn depth_two_shrink_preserves_bytes_and_reclaims_every_tree_node() {
    let path = scratch("depth2");
    let device = Arc::new(FileDevice::open_rw(path.to_str().unwrap()).unwrap());
    let fs = Filesystem::mount(device.clone()).unwrap();
    let ino = fs.apply_create("/fragmented", 0o600).unwrap();
    for n in 0..1500u64 {
        fs.apply_pwrite("/fragmented", n * 8192, &vec![(n % 251) as u8; 4096])
            .unwrap();
    }
    let (original, _) = fs.read_inode_verified(ino).unwrap();
    assert!(ExtentHeader::parse(&original.block).unwrap().depth >= 2);
    let target = 900 * 8192 + 19;
    fs.apply_truncate_shrink(ino, target).unwrap();
    let (after, _) = fs.read_inode_verified(ino).unwrap();
    let mut last = [0; 19];
    fs_ext4::file_io::read(&fs, &after, target - 19, 19, &mut last).unwrap();
    assert_eq!(last, [(900 % 251) as u8; 19]);
    fs.apply_truncate_grow(ino, target + 1024).unwrap();
    let (grown, _) = fs.read_inode_verified(ino).unwrap();
    let mut tail = [1; 1024];
    fs_ext4::file_io::read(&fs, &grown, target, 1024, &mut tail).unwrap();
    assert_eq!(tail, [0; 1024]);
    oracle(&path);
    fs.apply_unlink("/fragmented").unwrap();
    oracle(&path);
    drop(fs);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn densely_written_fragmented_file_finishes_and_can_be_removed() {
    let path = scratch("dense-fragmented");
    let device = Arc::new(FileDevice::open_rw(path.to_str().unwrap()).unwrap());
    let fs = Filesystem::mount(device.clone()).unwrap();
    let ino = fs.apply_create("/efisp.fat", 0o600).unwrap();
    fs.apply_create("/unrelated", 0o600).unwrap();
    for n in 0..80u64 {
        fs.apply_pwrite("/efisp.fat", n * 4096, &vec![(n % 251) as u8; 4096])
            .unwrap();
        fs.apply_pwrite("/unrelated", n * 4096, &[0x5a; 4096])
            .unwrap();
    }
    let (file, _) = fs.read_inode_verified(ino).unwrap();
    assert!(ExtentHeader::parse(&file.block).unwrap().depth > 0);
    let before = fs_ext4_test_support::snapshot_file(&path).unwrap();
    fs.apply_truncate_shrink(ino, file.size).unwrap();
    fs_ext4_test_support::assert_files_equal(&path, &before);
    fs.apply_unlink("/efisp.fat").unwrap();
    let (unrelated, _) = fs.read_inode_verified(inode(&fs, "/unrelated")).unwrap();
    assert_eq!(
        fs_ext4::file_io::read_all(&fs, &unrelated).unwrap(),
        vec![0x5a; 80 * 4096]
    );
    oracle(&path);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn invalid_deep_checksum_refuses_shrink_and_unlink_without_writing() {
    let path = scratch("corrupt");
    let device = Arc::new(FileDevice::open_rw(path.to_str().unwrap()).unwrap());
    let fs = Filesystem::mount(device.clone()).unwrap();
    let ino = inode(&fs, "/sparse.bin");
    let (entry, _) = fs.read_inode_verified(ino).unwrap();
    let child = fs_ext4::extent::ExtentIdx::parse(&entry.block[12..24])
        .unwrap()
        .leaf_block;
    drop(fs);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.seek(SeekFrom::Start(child * 4096 + 4095)).unwrap();
    let mut byte = [0];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 1;
    file.seek(SeekFrom::Start(child * 4096 + 4095)).unwrap();
    file.write_all(&byte).unwrap();
    let fs = Filesystem::mount(device.clone()).unwrap();
    let before = fs_ext4_test_support::snapshot_file(&path).unwrap();
    assert!(matches!(
        fs.apply_truncate_shrink(ino, 0),
        Err(fs_ext4::Error::BadChecksum { .. })
    ));
    let after_shrink = outside_state(&path);
    let outside_before = outside_state(&before);
    fs_ext4_test_support::assert_files_equal(&after_shrink, &outside_before);
    let unlinked = fs.apply_unlink("/sparse.bin");
    assert!(
        matches!(unlinked, Err(fs_ext4::Error::BadChecksum { .. })),
        "{unlinked:?}"
    );
    let after_unlink = outside_state(&path);
    fs_ext4_test_support::assert_files_equal(&after_unlink, &outside_before);
    drop(fs);
    fs_ext4_test_support::assert_files_equal(&path, &before);
    std::fs::remove_file(path).unwrap();
}

struct Interrupted {
    device: Arc<dyn BlockDevice>,
    writes: std::sync::atomic::AtomicUsize,
    budget: std::sync::atomic::AtomicUsize,
}
impl fs_ext4::block_io::BlockDevice for Interrupted {
    fn size_bytes(&self) -> u64 {
        self.device.size_bytes()
    }
    fn is_writable(&self) -> bool {
        true
    }
    fn read_at(&self, offset: u64, out: &mut [u8]) -> fs_ext4::Result<()> {
        self.device.read_at(offset, out)
    }
    fn write_at(&self, offset: u64, bytes: &[u8]) -> fs_ext4::Result<()> {
        use std::sync::atomic::Ordering::Relaxed;
        if self.writes.fetch_add(1, Relaxed) >= self.budget.load(Relaxed) {
            return Err(std::io::Error::other("injected interruption").into());
        }
        self.device.write_at(offset, bytes)
    }
}

#[test]
fn deep_shrink_interrupted_at_each_write_recovers_original_or_complete() {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    let path = scratch("interrupt");
    let source = path.clone();
    let mut writes = usize::MAX;
    // The first successful pass discovers the real transaction write count.
    // Then cut every write boundary and check the independent ext4 oracle.
    for budget in std::iter::once(usize::MAX).chain(0..64) {
        if budget != usize::MAX && budget > writes {
            break;
        }
        let image = PathBuf::from(fs_ext4_test_support::temp_path!(
            "ext4-deep-interrupt-{budget}-{}.img",
            std::process::id()
        ));
        fs_ext4_test_support::copy_file_sparse(&source, &image).unwrap();
        let device = Arc::new(FileDevice::open_rw(image.to_str().unwrap()).unwrap());
        let fault = Arc::new(Interrupted {
            device: device.clone(),
            writes: AtomicUsize::new(0),
            budget: AtomicUsize::new(usize::MAX),
        });
        let fs = Filesystem::mount(fault.clone()).unwrap();
        let ino = inode(&fs, "/sparse.bin");
        let (old, _) = fs.read_inode_verified(ino).unwrap();
        fault.writes.store(0, Relaxed);
        fault.budget.store(budget, Relaxed);
        let result = fs.apply_truncate_shrink(ino, 65_537);
        if budget == usize::MAX {
            result.unwrap();
            writes = fault.writes.load(Relaxed);
            assert!(writes < 64);
        } else if budget < writes {
            assert!(result.is_err());
        }
        drop(fs);
        let recovered = Filesystem::mount(device.clone()).unwrap();
        let (after, _) = recovered.read_inode_verified(ino).unwrap();
        assert!(
            after.size == old.size || after.size == 65_537,
            "torn inode at write {budget}"
        );
        drop(recovered);
        oracle(&image);
        std::fs::remove_file(image).unwrap();
    }
    std::fs::remove_file(path).unwrap();
}
