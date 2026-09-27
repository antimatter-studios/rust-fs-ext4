//! A hole below the first entry of a deep extent tree reads as zeros (#260).
//!
//! `descend` kept the last index entry whose `ei_block` is at or below the
//! block being mapped, and refused the map when there was none — which is
//! every block before the first one a sparse file holds. So an ordinary file
//! whose data starts past block 0 could not be read at its leading hole, and
//! the error said the extent tree was corrupt on a volume `e2fsck` accepts.
//!
//! The kernel's `ext4_ext_binsearch_idx` leaves the path at the first index
//! entry in that case, descends, finds no extent covering the block, and
//! reports a hole.
//!
//! Writing into that hole is the other half: the new extent becomes its
//! leaf's first, and every index entry naming the leaf holds the leaf's first
//! logical block. Leaving them as they were describes a tree e2fsck rejects
//! with "Logical start N does not match logical start M at next level", so
//! the keys are corrected up the path, as `ext4_ext_correct_indexes` does.
//!
//! Here a file is written one block every 16 blocks from logical block 16
//! upward, so the tree is deeper than the inode and nothing covers block 0.
//! `mkfs.ext4` and `e2fsck` judge it, from the harness VM they live in.

use fs_ext4::block_io::FileDevice;
use fs_ext4::file_io;
use fs_ext4::fs::Filesystem;
use fs_ext4_test_support::oracle;
use std::sync::Arc;

#[test]
fn a_hole_below_the_first_index_entry_reads_as_zeros() {
    let image = fs_ext4_test_support::temp_path!("fs_ext4_260_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(128 * 1024 * 1024))
        .unwrap();
    let mkfs = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096"])
        .arg(&image)
        .output();
    assert!(
        mkfs.status.success(),
        "{}",
        String::from_utf8_lossy(&mkfs.stderr)
    );

    let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
    let ino = fs.apply_create("/sparse", 0o644).expect("create");
    // Nothing at block 0: the first extent is at logical block 16, and one
    // block every 16 keeps each of them a record of its own.
    for i in 1..800u64 {
        fs.apply_pwrite("/sparse", i * 16 * 4096, &[7u8; 4096])
            .unwrap_or_else(|e| panic!("write block {}: {e:?}", i * 16));
    }
    let (inode, _) = fs.read_inode_verified(ino).unwrap();
    let depth = u16::from_le_bytes(inode.block[6..8].try_into().unwrap());
    assert!(
        depth >= 1,
        "the tree is still inline, so nothing here descends an index"
    );

    let mut buf = vec![0xFFu8; 4096];
    for hole in [0u64, 4096, 15 * 4096] {
        let n = file_io::read(&fs, &inode, hole, 4096, &mut buf)
            .unwrap_or_else(|e| panic!("reading the hole at {hole}: {e:?}"));
        assert_eq!(n, 4096, "short read at {hole}");
        assert!(
            buf.iter().all(|&b| b == 0),
            "the hole at {hole} read back {:?}",
            &buf[..8]
        );
    }

    // The data past it still reads, which is what says the descent did not
    // simply stop looking.
    let n = file_io::read(&fs, &inode, 16 * 4096, 4096, &mut buf).expect("read the first block");
    assert_eq!(n, 4096);
    assert!(buf.iter().all(|&b| b == 7), "{:?}", &buf[..8]);

    drop(fs);

    // Writing into the leading hole moves the first leaf's first key, and
    // the index entries above it have to follow.
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
        fs.apply_pwrite("/sparse", 0, &[9u8; 4096])
            .expect("write into the leading hole");
    }
    let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
    let (inode, _) = fs.read_inode_verified(ino).unwrap();
    let n = file_io::read(&fs, &inode, 0, 4096, &mut buf).expect("read what was just written");
    assert_eq!(n, 4096);
    assert!(buf.iter().all(|&b| b == 9), "{:?}", &buf[..8]);
    drop(fs);

    fs_ext4_test_support::assert_e2fsck_clean(&image, "leading hole");
    let _ = std::fs::remove_file(&image);
}
