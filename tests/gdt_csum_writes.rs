//! A volume with `GDT_CSUM` and not `METADATA_CSUM` is written correctly.
//!
//! That combination is what `mke2fs` produced by default before 1.43. Its
//! group descriptors carry a crc16 (`crc16(~0, uuid || group || desc)`), not
//! the crc32c `METADATA_CSUM` uses. The driver used to write only the crc32c
//! form, and only when `METADATA_CSUM` was set, so every descriptor a write
//! touched on such a volume was left stale and writes had to be refused.
//!
//! The oracle is the real toolchain: `mkfs.ext4` makes the volume, the driver
//! writes to it, and `e2fsck -fn` must find nothing. Both descriptor sizes are
//! covered, because with `64bit` the crc16 also covers the bytes after the
//! checksum field and without it they are left out. The volume spans several
//! groups so directory spreading reaches groups that are still `INODE_UNINIT`
//! and `BLOCK_UNINIT`, which is where the descriptor edits happen.
//!
//! `mkfs.ext4` and `e2fsck` run in the harness VM.

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::features::RoCompat;
use fs_ext4::{Error, Filesystem};
use fs_ext4_test_support::oracle;
use std::sync::Arc;

/// A fresh `mkfs.ext4` volume with `GDT_CSUM` and not `METADATA_CSUM`.
fn make_volume(tag: &str, sixty_four: bool) -> String {
    make_sized_volume(tag, sixty_four, 1024, 64)
}

fn make_sized_volume(tag: &str, sixty_four: bool, block_size: u32, mib: u64) -> String {
    let mkfs = "mkfs.ext4";
    let path =
        fs_ext4_test_support::temp_path!("fs_ext4_gdt_csum_{tag}_{}.img", std::process::id());
    std::fs::File::create(&path)
        .and_then(|f| f.set_len(mib * 1024 * 1024))
        .expect("size the image");
    let features = if sixty_four {
        "^metadata_csum,uninit_bg,64bit"
    } else {
        "^metadata_csum,uninit_bg,^64bit"
    };
    let out = oracle(mkfs)
        .args([
            "-q",
            "-F",
            "-b",
            &block_size.to_string(),
            "-O",
            features,
            "-E",
            "lazy_itable_init=1",
        ])
        .arg(&path)
        .output();
    assert!(
        out.status.success(),
        "mkfs.ext4 failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

fn write_and_check(tag: &str, sixty_four: bool) {
    let path = make_volume(tag, sixty_four);
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&path).expect("open_rw")))
            .expect("mount");
        assert_ne!(
            fs.sb.feature_ro_compat & RoCompat::GDT_CSUM.bits(),
            0,
            "[{tag}] gdt_csum"
        );
        assert_eq!(
            fs.sb.feature_ro_compat & RoCompat::METADATA_CSUM.bits(),
            0,
            "[{tag}] metadata_csum must be off"
        );
        assert_eq!(
            fs.sb.desc_size,
            if sixty_four { 64 } else { 32 },
            "[{tag}] desc_size"
        );
        assert!(fs.groups.len() > 2, "[{tag}] needs several groups");

        for d in 0..6 {
            let dir = format!("/d{d}");
            fs.apply_mkdir(&dir, 0o755).expect("mkdir");
            for f in 0..4 {
                let file = format!("{dir}/f{f}");
                fs.apply_create(&file, 0o644).expect("create");
                fs.apply_pwrite(&file, 0, &vec![d as u8 ^ f as u8; 20_000])
                    .expect("pwrite");
            }
        }
        fs.apply_unlink("/d0/f0").expect("unlink");
        fs.apply_rmdir("/d5")
            .expect_err("rmdir of a non-empty dir is refused");
        fs.dev.flush().expect("flush");
    }
    // The exit status is not enough on its own: `e2fsck -n` answers "no" to
    // "One or more block group descriptor checksums are invalid", prints
    // IGNORED, and still exits 0 when nothing else is wrong. The verdict
    // reads the report.
    fs_ext4_test_support::assert_e2fsck_clean(&path, &format!("{tag}: after the driver's writes"));

    // The mount checks what it reads: a descriptor whose crc16 no longer
    // matches is refused, as the kernel refuses it.
    {
        let dev = FileDevice::open_rw(&path).expect("open_rw");
        let at = 2 * 1024 + 0x0C; // group 0's free-blocks count, 1 KiB blocks
        let mut b = [0u8; 1];
        dev.read_at(at, &mut b).unwrap();
        dev.write_at(at, &[b[0] ^ 1]).unwrap();
        dev.flush().unwrap();
    }
    match Filesystem::mount(Arc::new(FileDevice::open(&path).expect("open"))) {
        Err(Error::BadChecksum { .. }) => {}
        other => panic!(
            "[{tag}] a stale crc16 must refuse the mount, got {:?}",
            other.map(|_| ())
        ),
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn gdt_csum_32_byte_descriptors_survive_e2fsck() {
    write_and_check("32", false);
}

#[test]
fn gdt_csum_64_byte_descriptors_survive_e2fsck() {
    write_and_check("64", true);
}

/// One large write that has to spread across several `BLOCK_UNINIT` groups,
/// then a rename, an unlink and an rmdir, and a small file written first that
/// must come back byte for byte.
///
/// A 32 MiB write on 1 KiB blocks does not fit in one 8 MiB group, so the
/// allocator stages several runs, and an extent-tree block, into groups whose
/// uninit flag the same transaction has only cleared on its buffer. If a
/// later plan in that transaction still sees the flag it rebuilds the bitmap
/// without the runs already staged and hands them out again; `e2fsck` then
/// reports blocks claimed twice, and `debugfs` reads the overwritten data.
/// At 4 KiB the same 128 MiB is one group: the crc16 path on a large block
/// size, with no uninit group for the write to reach.
fn large_write_and_check(tag: &str, sixty_four: bool, block_size: u32) {
    let path = make_sized_volume(tag, sixty_four, block_size, 128);
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&path).expect("open_rw")))
            .expect("mount");
        assert_ne!(
            fs.sb.feature_ro_compat & RoCompat::GDT_CSUM.bits(),
            0,
            "[{tag}] gdt_csum"
        );
        assert!(!fs.csum.enabled, "[{tag}] metadata_csum must be off");
        fs.apply_create("/unrelated.txt", 0o600).expect("create");
        fs.apply_pwrite("/unrelated.txt", 0, b"preserve this")
            .expect("pwrite");
        fs.apply_mkdir("/temporary", 0o755).expect("mkdir");
        fs.apply_create("/temporary/stage.bin", 0o600)
            .expect("create");
        fs.apply_pwrite("/temporary/stage.bin", 0, &vec![0x5a; 32 * 1024 * 1024])
            .expect("pwrite 32 MiB");
        fs.apply_rename("/temporary/stage.bin", "/large.bin", false)
            .expect("rename");
        fs.apply_create("/temporary/remove", 0o600).expect("create");
        fs.apply_unlink("/temporary/remove").expect("unlink");
        fs.apply_rmdir("/temporary").expect("rmdir");
        fs.dev.flush().expect("flush");
    }
    fs_ext4_test_support::assert_e2fsck_clean(&path, &format!("{tag}: after the driver's writes"));
    let out = oracle("debugfs")
        .args(["-R", "cat /unrelated.txt", &path])
        .output();
    assert_eq!(
        out.stdout,
        b"preserve this",
        "[{tag}] debugfs read back something else: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn gdt_csum_large_write_32_byte_1k() {
    large_write_and_check("large32_1k", false, 1024);
}

#[test]
fn gdt_csum_large_write_64_byte_1k() {
    large_write_and_check("large64_1k", true, 1024);
}

#[test]
fn gdt_csum_large_write_32_byte_4k() {
    large_write_and_check("large32_4k", false, 4096);
}

#[test]
fn gdt_csum_large_write_64_byte_4k() {
    large_write_and_check("large64_4k", true, 4096);
}

/// The kernel's `crc16` is CRC-16/ARC's polynomial; seeded with `~0` it is
/// CRC-16/MODBUS, whose published check value over "123456789" is 0x4B37.
#[test]
fn crc16_matches_the_published_check_value() {
    assert_eq!(fs_ext4::checksum::crc16(!0, b"123456789"), 0x4B37);
    assert_eq!(fs_ext4::checksum::crc16(0, b"123456789"), 0xBB3D);
}
