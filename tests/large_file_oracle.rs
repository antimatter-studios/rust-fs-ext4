//! `large_file`, judged by e2fsprogs in the guest.
//!
//! `tests/large_file_feature.rs` checks the flag with this crate's own
//! superblock reader. That proves the bit is where the reader looks; it
//! cannot prove the bit is the one `e2fsck` wants, or that `mke2fs` sets
//! it by default. These do:
//!
//! - every flavor this crate formats carries `large_file` exactly when
//!   `mke2fs -t <flavor>` does, read from both with `dumpe2fs -h`;
//! - a volume without the flag, given a file past 2 GiB - 1 by this
//!   crate, passes `e2fsck -fn`.

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::features::{FsFlavor, RoCompat};
use fs_ext4::{mkfs, Filesystem};
use fs_ext4_test_support::{assert_e2fsck_clean, oracle, temp_path};
use std::sync::Arc;

const SIZE: u64 = 32 << 20;

fn scratch(tag: &str, who: &str) -> String {
    let path = temp_path!("fs_ext4_large_file_{who}_{tag}_{}.img", std::process::id());
    std::fs::File::create(&path)
        .and_then(|f| f.set_len(SIZE))
        .unwrap_or_else(|e| panic!("create {path}: {e}"));
    path
}

fn ours(tag: &str, flavor: FsFlavor) -> String {
    let path = scratch(tag, "ours");
    let dev = FileDevice::open_rw(&path).expect("open_rw");
    mkfs::format_filesystem_with_flavor(&dev, None, None, SIZE, 4096, flavor).expect("format");
    dev.flush().expect("flush");
    path
}

/// The `Filesystem features:` line of `dumpe2fs -h`, as a list.
fn features(image: &str) -> Vec<String> {
    let out = oracle("dumpe2fs")
        .args(["-h", image])
        .judged()
        .clean("dumpe2fs");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.lines()
        .find_map(|l| l.strip_prefix("Filesystem features:"))
        .unwrap_or_else(|| panic!("dumpe2fs -h {image}: no features line in\n{text}"))
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_flavor_matches_mke2fs_on_large_file() {
    for (tag, flavor, kind) in [
        ("ext2", FsFlavor::Ext2, "ext2"),
        ("ext3", FsFlavor::Ext3, "ext3"),
        ("ext4", FsFlavor::Ext4, "ext4"),
    ] {
        let theirs = scratch(tag, "mke2fs");
        let made = oracle("mke2fs")
            .args(["-q", "-F", "-b", "4096", "-t", kind])
            .arg(&theirs)
            .output();
        assert!(
            made.status.success(),
            "[{tag}] mke2fs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        let mine = ours(tag, flavor);
        let has = |img: &str| features(img).iter().any(|f| f == "large_file");
        assert!(has(&theirs), "[{tag}] mke2fs sets large_file by default");
        assert!(has(&mine), "[{tag}] so does this crate's mkfs");
        let _ = std::fs::remove_file(&theirs);
        let _ = std::fs::remove_file(&mine);
    }
}

#[test]
fn a_file_past_2_gib_on_a_volume_without_the_flag_passes_e2fsck() {
    let path = ours("grown", FsFlavor::Ext4);
    {
        // Clear the flag, as a volume formatted without it has it.
        let dev = FileDevice::open_rw(&path).expect("open_rw");
        let mut sb = vec![0u8; 1024];
        dev.read_at(1024, &mut sb).expect("read superblock");
        let ro = u32::from_le_bytes(sb[0x64..0x68].try_into().unwrap());
        sb[0x64..0x68].copy_from_slice(&(ro & !RoCompat::LARGE_FILE.bits()).to_le_bytes());
        let c = fs_ext4::checksum::linux_crc32c(!0, &sb[..0x3FC]);
        sb[0x3FC..0x400].copy_from_slice(&c.to_le_bytes());
        dev.write_at(1024, &sb).expect("write superblock");
        dev.flush().expect("flush");
    }
    assert!(
        !features(&path).iter().any(|f| f == "large_file"),
        "the flag was cleared for the test"
    );
    {
        let dev = FileDevice::open_rw(&path).expect("open_rw");
        let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
        fs.apply_create("/big", 0o644).expect("create");
        fs.apply_pwrite("/big", 3 << 30, b"tail")
            .expect("pwrite past 2 GiB");
    }
    assert_e2fsck_clean(&path, "large_file_grown");
    let _ = std::fs::remove_file(&path);
}
