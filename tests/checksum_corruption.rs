//! Verify that mounting an ext4 image with a corrupted superblock checksum
//! is rejected when METADATA_CSUM is enabled.
//!
//! Strategy: copy ext4-basic.img into a tempfile, flip one byte inside the
//! superblock region (at offset 1024 + 0x100 — the first byte of the
//! `s_volume_name` field, which is covered by the checksum but not used by
//! the magic check), and confirm `Filesystem::mount` returns
//! `Error::BadChecksum`.

use fs_ext4::block_io::FileDevice;
use fs_ext4::error::Error;
use fs_ext4::fs::Filesystem;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

const SRC_IMAGE: &str = "ext4-basic.img";

fn copy_image_to_temp(tag: &str) -> std::path::PathBuf {
    let src_path = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), SRC_IMAGE);
    let tmp_path = fs_ext4_test_support::temp_dir().join(format!(
        "ext4rs-corrupt-{}-{}.img",
        std::process::id(),
        tag
    ));
    fs_ext4_test_support::copy_file_sparse(&src_path, &tmp_path).expect("copy image");
    tmp_path
}

#[test]
fn pristine_image_mounts_cleanly() {
    let tmp = copy_image_to_temp("pristine");
    let dev = Arc::new(FileDevice::open(tmp.to_str().unwrap()).expect("open temp image"));
    let fs = Filesystem::mount(dev).expect("mount pristine copy");
    // ext4-basic.img is built with metadata_csum.
    assert!(fs.csum.enabled, "METADATA_CSUM not enabled in {SRC_IMAGE}");
    let _ = std::fs::remove_file(tmp);
}

#[test]
fn corrupted_superblock_is_rejected() {
    let tmp = copy_image_to_temp("corrupt");

    // Probe that checksum is enabled before corrupting (ext4-basic.img is
    // built with metadata_csum).
    {
        let dev = Arc::new(FileDevice::open(tmp.to_str().unwrap()).expect("probe open"));
        let fs = Filesystem::mount(dev).expect("probe mount");
        assert!(fs.csum.enabled, "METADATA_CSUM not enabled in {SRC_IMAGE}");
    }

    // Flip one bit inside the superblock-checksum-covered region.
    // Offset 1024 (start of superblock) + 0x100 = byte 0x500 in image.
    // 0x100 lands inside s_volume_name (covered by checksum, not by magic).
    {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&tmp)
            .expect("reopen rw");
        f.seek(SeekFrom::Start(1024 + 0x100)).unwrap();
        let mut byte = [0u8; 1];
        f.read_exact(&mut byte).unwrap();
        byte[0] ^= 0xFF;
        f.seek(SeekFrom::Start(1024 + 0x100)).unwrap();
        f.write_all(&byte).unwrap();
    }

    let dev = Arc::new(FileDevice::open(tmp.to_str().unwrap()).expect("open corrupted"));
    match Filesystem::mount(dev) {
        Err(Error::BadChecksum { what }) => {
            assert_eq!(what, "superblock");
        }
        Ok(_) => panic!("mount accepted corrupted superblock"),
        Err(e) => panic!("mount returned wrong error: {e:?}"),
    }

    let _ = std::fs::remove_file(tmp);
}
