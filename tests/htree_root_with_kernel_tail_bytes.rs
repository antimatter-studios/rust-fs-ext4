//! An htree root that ends in dirent-tail-shaped bytes is still an index
//! (#233).
//!
//! When the kernel turns a linear directory into an htree, it keeps the old
//! dirent tail's first bytes in `dx_tail.dt_reserved`: `0c 00 00 de`, a
//! record length of 12 and the 0xDE marker. So a kernel-grown dx_root ends
//! in what looks exactly like a `dir_entry_tail`, whose "checksum" is the
//! index's. The driver decided which checksum to verify by that shape. It
//! refused every create in such a directory as a bad directory block, and
//! every unlink as a bad record length, while e2fsck accepted the volume.
//! `all_images_rw_smoke::ext4_manyfiles` hit this on the CI image the kernel
//! built.
//!
//! This builds the same bytes without a kernel. `mkfs.ext4 -d` writes 512
//! files, `e2fsck -fyD` indexes the root, and the root's `dt_reserved` is set
//! to the kernel's value with the index checksum restamped. `e2fsck -fn` must
//! accept that as built. Then a create, a write and two unlinks go through,
//! and e2fsck accepts the result. the e2fsprogs tools run in the harness VM.

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use std::sync::Arc;

fn e2fsck_clean(image: &str, what: &str) {
    fs_ext4_test_support::assert_e2fsck_clean(image, what);
}

/// An image with a 512-entry root that `e2fsck -D` indexed, and its path.
fn indexed_root(tag: &str) -> (String, String) {
    let root = fs_ext4_test_support::temp_path!("fs_ext4_233_{}_{}", tag, std::process::id());
    std::fs::create_dir_all(&root).unwrap();
    for i in 1..=512 {
        std::fs::write(format!("{root}/file_{i}.txt"), format!("f{i:04}\n")).unwrap();
    }
    let image = format!("{root}.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(16 * 1024 * 1024))
        .unwrap();
    let mkfs = fs_ext4_test_support::oracle("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-b",
            "4096",
            "-O",
            "dir_index,metadata_csum",
            "-d",
            &root,
        ])
        .arg(&image)
        .output();
    assert!(
        mkfs.status.success(),
        "{}",
        String::from_utf8_lossy(&mkfs.stderr)
    );
    fs_ext4_test_support::oracle("e2fsck")
        .args(["-fyD", &image])
        .judged()
        .repaired("e2fsck -fyD indexes the root");
    (root, image)
}

#[test]
fn a_root_whose_dx_tail_looks_like_a_dirent_tail_takes_creates_and_unlinks() {
    let (root, image) = indexed_root("tail");

    // The kernel's bytes in dt_reserved, with the index checksum restamped.
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
        let (dir, _) = fs.read_inode_verified(2).unwrap();
        assert!(
            dir.flags & fs_ext4::inode::InodeFlags::INDEX.bits() != 0,
            "e2fsck -D did not index the root"
        );
        let phys = fs.map_inode_logical(&dir, 0).unwrap().unwrap();
        let mut block = fs.read_block(phys).unwrap();
        let limit = u16::from_le_bytes([block[32], block[33]]) as usize;
        let tail = 32 + limit * 8;
        assert_eq!(
            tail,
            block.len() - 8,
            "the dx_tail is not at the block's end"
        );
        block[tail..tail + 4].copy_from_slice(&[0x0c, 0x00, 0x00, 0xde]);
        fs.csum.patch_dx_tail(2, dir.generation, &mut block, 32);
        assert!(
            fs_ext4::dir::has_csum_tail(&block),
            "the root does not end in a dirent-tail shape, so this tests nothing"
        );
        fs.dev
            .write_at(phys * u64::from(fs.sb.block_size()), &block)
            .unwrap();
    }
    e2fsck_clean(&image, "as built");

    let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
    fs.apply_create("/new.txt", 0o644).expect("create");
    fs.apply_pwrite("/new.txt", 0, b"hello").expect("write");
    fs.apply_unlink("/new.txt").expect("unlink the new file");
    fs.apply_unlink("/file_7.txt")
        .expect("unlink a file the index already held");
    drop(fs);
    e2fsck_clean(&image, "after create and unlinks");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&image);
}

/// A leaf whose first record is an empty entry spanning the block, with no
/// dirent tail, has the shape of an htree node. A leaf emptied on a volume
/// without checksums looks like this, and so does one written before
/// `metadata_csum` was enabled. The kernel does not verify a node-shaped
/// block that a linear scan reads (`__ext4_read_dirblock` with
/// `DIRENT`), so a create's existence scan must not refuse it as a bad
/// index block (CodeRabbit on #256).
#[test]
fn a_node_shaped_leaf_does_not_stop_a_create() {
    let (root, image) = indexed_root("leaf");
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
        let (dir, _) = fs.read_inode_verified(2).unwrap();
        // Block 0 is the root; the last block is a leaf, never a node, in a
        // one-level index of 512 names.
        let last = dir.size / u64::from(fs.sb.block_size()) - 1;
        let phys = fs.map_inode_logical(&dir, last).unwrap().unwrap();
        let mut block = vec![0u8; fs.sb.block_size() as usize];
        let len = block.len() as u16;
        block[4..6].copy_from_slice(&len.to_le_bytes());
        fs.dev
            .write_at(phys * u64::from(fs.sb.block_size()), &block)
            .unwrap();
    }
    let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
    fs.apply_create("/after_the_empty_leaf.txt", 0o644)
        .expect("create past a node-shaped leaf");
    drop(fs);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&image);
}
