//! What a mount does with numbers the image chose.
//!
//! Every field patched below is one an image supplies and the driver
//! used as a size, a count, or a loop bound. They are patched into
//! `ext4-no-csum.img` because that image has no metadata checksums, so
//! a single field can be changed without also having to restamp a CRC
//! the driver would otherwise reject first — the point is what happens
//! *after* the field is accepted, not whether a checksum catches it.
//!
//! Each test says which refusal it expects, not merely that there was
//! one. Several of these fail either way once a read runs off the end
//! of the image; what is being asserted is that they are refused before
//! the buffer is allocated or the loop is entered.

use fs_ext4::block_io::FileDevice;
use fs_ext4::Filesystem;
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;

const SUPERBLOCK_AT: u64 = 1024;

/// Superblock field offsets, from the ext4 on-disk layout.
mod sb {
    pub const BLOCKS_COUNT_LO: u64 = 0x04;
    pub const LOG_BLOCK_SIZE: u64 = 0x18;
    pub const BLOCKS_PER_GROUP: u64 = 0x20;
    pub const DESC_SIZE: u64 = 0xFE;
}

fn copy_to_tmp(tag: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let src = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), "ext4-no-csum.img");
    let dst =
        fs_ext4_test_support::temp_path!("fs_ext4_hostile_{}_{n}_{tag}.img", std::process::id());
    fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy {src} -> {dst}: {e}"));
    dst
}

fn patch(path: &str, field: u64, bytes: &[u8]) {
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    f.seek(SeekFrom::Start(SUPERBLOCK_AT + field)).unwrap();
    f.write_all(bytes).unwrap();
    f.flush().unwrap();
}

fn mount_error(path: &str) -> String {
    let dev = FileDevice::open(path).expect("open");
    format!("{:?}", Filesystem::mount(Arc::new(dev)).err())
}

/// ext4 tops out at a 64 KiB block. The guard admitted 20 — a 1 GiB
/// block — on the argument that anything larger was certainly corrupt,
/// which is true of 20 as well. Every `vec![0u8; block_size]` in the
/// crate is sized by this field, and the mount wraps the device in a
/// 256-entry block cache, so 1 GiB blocks are 256 GiB of resident
/// memory from a sparse image.
#[test]
fn a_block_larger_than_ext4_defines_is_refused() {
    let path = copy_to_tmp("bigblock");
    patch(&path, sb::LOG_BLOCK_SIZE, &20u32.to_le_bytes());
    let why = mount_error(&path);
    assert!(
        why.contains("log_block_size exceeds the largest ext4 block"),
        "a 1 GiB block was refused as {why}"
    );
    fs::remove_file(path).ok();
}

/// The descriptor parser reads fixed offsets up to 0x20, and up to 0x3C
/// when the field says 64. A smaller value indexes past the buffer.
#[test]
fn a_descriptor_size_smaller_than_a_descriptor_is_refused() {
    let path = copy_to_tmp("descsize");
    patch(&path, sb::DESC_SIZE, &8u16.to_le_bytes());
    let why = mount_error(&path);
    assert!(
        why.contains("desc_size is not a group descriptor size"),
        "a descriptor size of 8 was refused as {why} -- before the change \
         this was `range end index 12 out of range for slice of length 8`"
    );
    fs::remove_file(path).ok();
}

/// `group_count` is `blocks_count / blocks_per_group`, and the table is
/// `group_count * desc_size` bytes read whole into one buffer.
#[test]
fn a_group_descriptor_table_larger_than_the_device_is_refused() {
    let path = copy_to_tmp("bgt");
    // 2^32 blocks in groups of one: four billion descriptors.
    patch(&path, sb::BLOCKS_COUNT_LO, &0xFFFF_FFFFu32.to_le_bytes());
    patch(&path, sb::BLOCKS_PER_GROUP, &1u32.to_le_bytes());
    let why = mount_error(&path);
    assert!(
        why.contains("group descriptor table reaches past the end"),
        "a 128 GiB descriptor table was refused as {why}, which means the \
         buffer was allocated first"
    );
    fs::remove_file(path).ok();
}

/// Where inode `ino` starts on the device, asked of the driver rather
/// than recomputed here, so the test cannot disagree with the parser
/// about the layout.
fn inode_offset(path: &str, ino: u32) -> u64 {
    let dev = FileDevice::open(path).expect("open");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
    let group = (ino - 1) / fs.sb.inodes_per_group;
    let index = (ino - 1) % fs.sb.inodes_per_group;
    fs.groups[group as usize].inode_table * fs.sb.block_size() as u64
        + index as u64 * fs.sb.inode_size as u64
}

/// Overwrite `i_size_high` (bytes 0x6C..0x70 of an inode).
fn set_size_high(path: &str, ino: u32, value: u32) {
    let at = inode_offset(path, ino) + 0x6C;
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(&value.to_le_bytes()).unwrap();
    f.flush().unwrap();
}

/// Every directory scan walks `0..size.div_ceil(block_size)` and steps
/// over a logical block that is not mapped, which is what the kernel
/// does too — so the loop is never ended by an error and never bounded
/// by real content. `i_size_high` on the root turned a lookup into a
/// scan of 2^32 blocks that was still running after twenty seconds,
/// with the entry cap never reached because no entry is ever found.
///
/// A regular file may legitimately declare more bytes than the
/// filesystem holds; that is what a sparse file is. A directory's
/// blocks are all really there.
#[test]
fn a_directory_larger_than_the_filesystem_is_refused() {
    let path = copy_to_tmp("bigdir");
    // Root inode, 2^44 bytes.
    set_size_high(&path, 2, 0x1000);

    let dev = FileDevice::open(&path).expect("open");
    let fs = Filesystem::mount(Arc::new(dev)).expect("the superblock is untouched");
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    let why = format!(
        "{:?}",
        fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, "/nothing-here").err()
    );
    assert!(
        why.contains("declares more bytes than the filesystem holds"),
        "a directory of 2^44 bytes was answered with {why}"
    );

    drop(fs);
    fs::remove_file(path).ok();
}

/// `read_all` materialises the file, and `i_size` is
/// `join32(i_size_high, i_size_lo)` off the disk with nothing comparing
/// it to anything. Setting `i_size_high` on a file in a 4 MiB image
/// reached `memory allocation of 2305843009213694048 bytes failed` and
/// took the process down: `handle_alloc_error` aborts, so the FFI
/// boundary's `catch_unwind` never sees it.
#[test]
fn reading_a_file_whole_will_not_ask_for_more_memory_than_the_filesystem_holds() {
    let path = copy_to_tmp("bigfile");
    let dev = FileDevice::open(&path).expect("open");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    let ino = fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, "/file.txt")
        .expect("ext4-no-csum.img carries /file.txt");
    drop(fs);

    set_size_high(&path, ino, 0x2000_0000);

    let dev = FileDevice::open(&path).expect("open");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
    let (inode, _) = fs.read_inode_verified(ino).expect("read inode");
    let why = format!("{:?}", fs_ext4::file_io::read_all(&fs, &inode).err());
    assert!(
        why.contains("more memory than the filesystem has bytes"),
        "a file of 2^61 bytes was answered with {why}"
    );

    drop(fs);
    fs::remove_file(path).ok();
}

/// Group-descriptor field offsets, from the ext4 on-disk layout.
mod bgd {
    pub const BLOCK_BITMAP_LO: u64 = 0x00;
    pub const INODE_TABLE_LO: u64 = 0x08;
}

/// Where the group descriptor table starts on the device.
fn descriptor_table_at(path: &str) -> u64 {
    let dev = FileDevice::open(path).expect("open");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
    (fs.sb.first_data_block as u64 + 1) * fs.sb.block_size() as u64
}

fn patch_descriptor(path: &str, group: u64, field: u64, value: u32) {
    let dev = FileDevice::open(path).expect("open");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
    let desc_size = fs.sb.desc_size as u64;
    drop(fs);
    let at = descriptor_table_at(path) + group * desc_size + field;
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(&value.to_le_bytes()).unwrap();
    f.flush().unwrap();
}

/// `bg_block_bitmap`, `bg_inode_bitmap` and `bg_inode_table` are block
/// numbers in this filesystem, and every one of them is a write target:
/// the bitmap writers put a whole block at the first two, and
/// `locate_inode` hands the third to `write_inode_raw`, which writes an
/// inode image there. None was checked against anything, and a
/// descriptor pointing outside the filesystem is trivially satisfied on
/// an FSKit mount, where the device is larger than `s_blocks_count`.
#[test]
fn a_group_descriptor_pointing_outside_the_filesystem_is_refused() {
    for (what, field) in [
        ("block bitmap", bgd::BLOCK_BITMAP_LO),
        ("inode table", bgd::INODE_TABLE_LO),
    ] {
        let path = copy_to_tmp("bgdptr");
        patch_descriptor(&path, 0, field, 0x00FF_FFFF);
        let why = mount_error(&path);
        assert!(
            why.contains("points outside the filesystem"),
            "a {what} at block 0x00FFFFFF was answered with {why}"
        );
        fs::remove_file(path).ok();
    }
}

/// One group of `u32::MAX` blocks passes the descriptor-table check --
/// the table is one descriptor -- and claimed ~16 TiB on an 8 MiB image
/// (#321). A group cannot have more blocks than its one bitmap block has
/// bits, which is the kernel's "blocks per group too big".
#[test]
fn a_group_larger_than_its_bitmap_is_refused() {
    let path = copy_to_tmp("bpg");
    patch(&path, sb::BLOCKS_COUNT_LO, &u32::MAX.to_le_bytes());
    patch(&path, sb::BLOCKS_PER_GROUP, &u32::MAX.to_le_bytes());
    let why = mount_error(&path);
    assert!(
        why.contains("Corrupt") && why.contains("blocks_per_group"),
        "one group of 2^32 blocks was answered with {why}"
    );
    fs::remove_file(path).ok();
}

/// A filesystem cannot be larger than the device it is on: every bound
/// that reads `blocks_count` then trusts a size nothing checked (#321).
/// Twice the image's blocks is still one group, so nothing else refuses
/// it; the kernel's "block count exceeds size of device" does.
#[test]
fn a_filesystem_larger_than_its_device_is_refused() {
    let path = copy_to_tmp("pastdev");
    let blocks = (fs::metadata(&path).unwrap().len() / 4096 * 2) as u32;
    patch(&path, sb::BLOCKS_COUNT_LO, &blocks.to_le_bytes());
    let why = mount_error(&path);
    assert!(
        why.contains("Corrupt") && why.contains("larger than the device"),
        "a filesystem of twice the device was answered with {why}"
    );
    fs::remove_file(path).ok();
}

/// The whole-file and directory bounds are the device, not only
/// `blocks_count * block_size`, and the product is checked: with a
/// superblock claiming 2^32 blocks a 3 TiB `i_size` was under the
/// `saturating_mul` bound, so `read_all` asked for a 3 TiB buffer and a
/// directory scan walked ~2^30 blocks (#321).
#[test]
fn whole_file_and_directory_bounds_are_the_device() {
    let path = copy_to_tmp("devbound");
    let dev = FileDevice::open(&path).expect("open");
    let mut fs = Filesystem::mount(Arc::new(dev)).expect("mount");
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    let ino = fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, "/file.txt")
        .expect("ext4-no-csum.img carries /file.txt");
    // What a mount that trusted the field would carry.
    fs.sb.blocks_count = u64::from(u32::MAX);

    let (mut inode, _) = fs.read_inode_verified(ino).expect("read inode");
    inode.size = 3 << 40;
    let why = format!("{:?}", fs_ext4::file_io::read_all(&fs, &inode).err());
    assert!(
        why.contains("Corrupt") && why.contains("more memory than the filesystem has bytes"),
        "a 3 TiB file on an 8 MiB device was answered with {why}"
    );

    // And a product that overflows is refused, not saturated.
    fs.sb.blocks_count = u64::MAX;
    let why = format!("{:?}", fs_ext4::file_io::read_all(&fs, &inode).err());
    assert!(why.contains("Corrupt"), "blocks_count 2^64-1 gave {why}");

    drop(fs);
    fs::remove_file(path).ok();
}

/// The directory half of the same bound: a 3 TiB root under a superblock
/// claiming 2^32 blocks passed `read_inode_verified`, and every scan of
/// it then walked `size / block_size` logical blocks (#321).
#[test]
fn a_directory_larger_than_the_device_is_refused() {
    let path = copy_to_tmp("devdir");
    // Root inode, 3 TiB.
    set_size_high(&path, 2, 0x300);
    let dev = FileDevice::open(&path).expect("open");
    let mut fs = Filesystem::mount(Arc::new(dev)).expect("the superblock is untouched");
    fs.sb.blocks_count = u64::from(u32::MAX);
    let why = format!("{:?}", fs.read_inode_verified(2).err());
    assert!(
        why.contains("declares more bytes than the filesystem holds"),
        "a 3 TiB directory on an 8 MiB device was answered with {why}"
    );
    drop(fs);
    fs::remove_file(path).ok();
}
