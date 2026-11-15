//! A volume without 64BIT reads 32-byte group descriptors whatever
//! `s_desc_size` says, as the kernel does (#140).
//!
//! The field was checked only against a floor, so on a non-64BIT volume a
//! value of 64 passed and the descriptor table was read at a 64-byte
//! stride. The image is a fresh `mkfs.ext4 -O ^64bit` (without
//! metadata_csum, so the superblock checksum needs no restamping) with the
//! field set to 64; fails when the harness VM the e2fsprogs tools run in is unreachable.

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::Filesystem;
use std::sync::Arc;

#[test]
fn a_non_64bit_volume_with_a_64_byte_desc_size_field_reads_its_real_descriptors() {
    let dir =
        fs_ext4_test_support::temp_dir().join(format!("ext4-desc-size-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("d.img");
    std::fs::File::create(&img)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let made = fs_ext4_test_support::oracle("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-b",
            "4096",
            "-g",
            "4096",
            "-O",
            "^64bit,^metadata_csum",
        ])
        .arg(&img)
        .output();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let path = img.to_str().unwrap();
    let mount =
        || Filesystem::mount(Arc::new(FileDevice::open(path).unwrap()) as Arc<dyn BlockDevice>);

    let before: Vec<(u64, u64, u64)> = mount()
        .expect("the untouched image mounts")
        .groups
        .iter()
        .map(|g| (g.block_bitmap, g.inode_bitmap, g.inode_table))
        .collect();
    assert!(before.len() > 1, "fixture: several groups");

    let mut bytes = fs_ext4_test_support::read_small_file(&img).unwrap();
    bytes[1024 + 0xFE..1024 + 0x100].copy_from_slice(&64u16.to_le_bytes());
    std::fs::write(&img, &bytes).unwrap();

    let fs = mount().unwrap_or_else(|e| panic!("Linux mounts this volume: {e:?}"));
    assert_eq!(fs.sb.desc_size, 32);
    let after: Vec<(u64, u64, u64)> = fs
        .groups
        .iter()
        .map(|g| (g.block_bitmap, g.inode_bitmap, g.inode_table))
        .collect();
    assert_eq!(
        after, before,
        "the descriptors read differently once the field said 64"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
