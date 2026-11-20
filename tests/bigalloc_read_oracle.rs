//! A bigalloc volume reads as e2fsprogs reads it, and is not written (#75).
//!
//! `mkfs.ext4 -O bigalloc -d` writes known files at two cluster ratios: 16
//! (`-C 65536`, one group) and 4 (`-C 16384`, several groups, so group
//! offsets past the first are exercised). Every file must read back
//! byte-identical to its source and to `debugfs cat`. A write must be
//! refused naming the feature, since the allocator counts blocks where the
//! bitmaps count clusters, and so must the audit, for the same reason.
//! The e2fsprogs tools run in the harness VM; a test fails when it cannot reach them.

#![cfg(unix)]

use fs_ext4::block_io::FileDevice;
use fs_ext4::features::RoCompat;
use fs_ext4::Filesystem;
use std::sync::Arc;

fn read(fs: &Filesystem, path: &str) -> Vec<u8> {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    let ino = fs_ext4::path::lookup_with_csum(fs.dev.as_ref(), &fs.sb, &mut reader, path, &fs.csum)
        .unwrap_or_else(|e| panic!("lookup {path}: {e:?}"));
    let (inode, raw) = fs.read_inode_verified(ino).unwrap();
    let mut out = vec![0u8; inode.size as usize];
    let n =
        fs_ext4::file_io::read_with_raw_verified(fs, &inode, &raw, ino, 0, inode.size, &mut out)
            .unwrap_or_else(|e| panic!("read {path}: {e:?}"));
    out.truncate(n as usize);
    out
}

/// Deterministic bytes that do not repeat on a block boundary.
fn content(seed: u32, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2654435761) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn bigalloc_volume(block: u32, cluster: u32, size_mib: u64, min_groups: usize) {
    let mkfs = "mkfs.ext4";
    let debugfs = "debugfs";
    let tag = format!("bigalloc_{block}_{cluster}");
    let root = fs_ext4_test_support::temp_path!("fs_ext4_{tag}_{}", std::process::id());
    std::fs::create_dir_all(format!("{root}/d/e")).unwrap();
    let mut files = Vec::new();
    for i in 1..=24u32 {
        // Sizes around and across cluster boundaries.
        let len = (i as usize * 7919) % (3 * cluster as usize) + i as usize;
        files.push((format!("/f{i}.bin"), content(i, len)));
    }
    files.push((
        "/d/big.bin".into(),
        content(99, (size_mib as usize * 1024 * 1024 / 8).min(6_000_000)),
    ));
    files.push(("/d/e/small".into(), b"hi\n".to_vec()));
    for (path, bytes) in &files {
        std::fs::write(format!("{root}{path}"), bytes).unwrap();
    }
    let image = format!("{root}.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(size_mib * 1024 * 1024))
        .unwrap();
    let out = fs_ext4_test_support::oracle(mkfs)
        .args(["-q", "-F", "-O", "bigalloc", "-b"])
        .arg(block.to_string())
        .arg("-C")
        .arg(cluster.to_string())
        .args(["-d", &root, &image])
        .output();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap()))
        .unwrap_or_else(|e| panic!("[{tag}] mount: {e:?}"));
    assert!(fs.sb.feature_ro_compat & RoCompat::BIGALLOC.bits() != 0);
    assert_eq!(fs.sb.block_size(), block, "[{tag}]");
    assert!(
        fs.groups.len() >= min_groups,
        "[{tag}] {} groups",
        fs.groups.len()
    );
    for (path, bytes) in &files {
        assert!(
            read(&fs, path) == *bytes,
            "[{tag}] {path} differs from its source"
        );
    }
    let dumped = format!("{root}/debugfs-big.bin");
    fs_ext4_test_support::oracle(debugfs)
        .args(["-R", &format!("dump /d/big.bin {dumped}"), &image])
        .judged()
        .clean("debugfs dump");
    fs_ext4_test_support::assert_files_equal(format!("{root}/d/big.bin"), dumped);

    let err = fs_ext4::fsck::audit(&fs, u32::MAX, u32::MAX).expect_err("audit");
    assert!(
        matches!(err, fs_ext4::Error::Unsupported(m) if m.contains("bigalloc")),
        "[{tag}] {err:?}"
    );
    drop(fs);

    let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap()))
        .unwrap_or_else(|e| panic!("[{tag}] mount rw: {e:?}"));
    let err = fs
        .apply_create("/new", 0o644)
        .expect_err("a write to a bigalloc volume");
    assert!(
        matches!(err, fs_ext4::Error::UnsupportedRoCompat(bits) if bits & RoCompat::BIGALLOC.bits() != 0),
        "[{tag}] {err:?}"
    );
    drop(fs);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&image);
}

/// `-C 65536` at 4 KiB: sixteen blocks to a cluster, the issue's fixture.
#[test]
fn a_bigalloc_volume_with_64k_clusters() {
    bigalloc_volume(4096, 65536, 256, 1);
}

/// `-C 16384` at 4 KiB: four blocks to a cluster, 512 MiB groups, four of
/// them in a sparse 1.6 GiB image.
#[test]
fn a_multi_group_bigalloc_volume() {
    bigalloc_volume(4096, 16384, 1600, 4);
}

/// `-C 16384` at 1 KiB: bigalloc puts the groups at block 0 while the
/// superblock stays in block 1, so the descriptor table is at block 2, not
/// `s_first_data_block + 1`.
#[test]
fn a_1k_bigalloc_volume() {
    bigalloc_volume(1024, 16384, 64, 1);
}
