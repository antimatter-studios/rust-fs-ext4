//! A write into a preallocated range lands in the preallocated blocks.
//!
//! `fallocate` without `KEEP_SIZE`'s growth, or an application reserving
//! space ahead of writing it, leaves an uninitialized extent: blocks that
//! belong to the file and read as zeros. `apply_pwrite` asked
//! `extent::map_logical` whether each block was mapped. That answers `None`
//! for an uninitialized extent, because a read must see zeros. The write
//! took `None` to mean a hole, allocated a fresh block and tried to insert
//! an extent over the range the uninitialized one already covers, and was
//! refused as a corrupt extent tree. The volume was valid; the write just
//! could not be made.
//!
//! Volumes come from `mkfs.ext4`, and `e2fsck -fn` must accept every result.
//! The e2fsprogs tools run in the harness VM; a test fails when it cannot reach them.

use fs_ext4::block_io::FileDevice;
use fs_ext4::file_io;
use fs_ext4::fs::Filesystem;
use std::sync::Arc;

fn mkfs(tag: &str) -> String {
    let mkfs = "mkfs.ext4";
    let path =
        fs_ext4_test_support::temp_path!("fs_ext4_prealloc_{tag}_{}.img", std::process::id());
    std::fs::File::create(&path)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .unwrap();
    let out = fs_ext4_test_support::oracle(mkfs)
        .args(["-q", "-F", "-b", "4096"])
        .arg(&path)
        .output();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

fn e2fsck_clean(path: &str) {
    fs_ext4_test_support::assert_e2fsck_clean(path, "e2fsck -fn");
}

fn mount(path: &str) -> Filesystem {
    Filesystem::mount(Arc::new(FileDevice::open_rw(path).unwrap())).unwrap()
}

fn ino_of(fs: &Filesystem, path: &str) -> u32 {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, path).unwrap()
}

/// `(offset, len)` writes into a 64 KiB preallocation, each on a fresh
/// volume: the whole range, its head, its tail, a block in the middle, and a
/// range starting inside it and running past its end.
#[test]
fn a_write_into_a_preallocated_range_lands_there() {
    const PREALLOC: u64 = 64 * 1024;
    let cases: [(&str, u64, usize); 5] = [
        ("whole", 0, PREALLOC as usize),
        ("head", 0, 100),
        ("tail", PREALLOC - 4096, 4096),
        ("middle", 5 * 4096 + 17, 5000),
        ("past_end", PREALLOC - 1000, 9000),
    ];
    for (tag, offset, len) in cases {
        let path = mkfs(tag);
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8 + 1).collect();
        let fs = mount(&path);
        let ino = fs.apply_create("/f", 0o644).expect("create");
        fs.apply_fallocate_keep_size(ino, 0, PREALLOC)
            .expect("preallocate");
        drop(fs);
        e2fsck_clean(&path);

        // What a preallocated block holds is whatever was there before.
        // Fill it, so a write that let any of it show would be caught.
        {
            let fs = mount(&path);
            let (inode, _) = fs.read_inode_verified(ino_of(&fs, "/f")).unwrap();
            let e = fs_ext4::extent::lookup(&inode.block, fs.dev.as_ref(), fs.sb.block_size(), 0)
                .unwrap()
                .expect("the preallocated extent");
            assert!(
                e.uninitialized,
                "[{tag}] the preallocation is not uninitialized"
            );
            fs.dev
                .write_at(e.physical_block * 4096, &vec![0xee; PREALLOC as usize])
                .unwrap();
        }

        let fs = mount(&path);
        let free_before = fs.sb.free_blocks_count;
        let size = fs
            .apply_pwrite("/f", offset, &data)
            .unwrap_or_else(|e| panic!("[{tag}] pwrite into the preallocation: {e:?}"));
        assert_eq!(size, offset + len as u64, "[{tag}] size");
        drop(fs);
        e2fsck_clean(&path);

        let fs = mount(&path);
        let ino = ino_of(&fs, "/f");
        let (inode, _) = fs.read_inode_verified(ino).unwrap();
        let got = file_io::read_all(&fs, &inode).unwrap();
        let mut want = vec![0u8; size as usize];
        want[offset as usize..].copy_from_slice(&data);
        assert!(
            got == want,
            "[{tag}] the file does not read back as written"
        );
        // Blocks inside the preallocation are reused, not allocated again.
        let beyond = (offset + len as u64)
            .saturating_sub(PREALLOC)
            .div_ceil(4096);
        assert!(
            free_before - fs.sb.free_blocks_count <= beyond,
            "[{tag}] allocated {} blocks for a write needing {beyond} past the preallocation",
            free_before - fs.sb.free_blocks_count
        );
        let _ = std::fs::remove_file(&path);
    }
}
