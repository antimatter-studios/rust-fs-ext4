//! How `apply_pwrite` cuts a large write into transactions (#293).
//!
//! A pwrite journals its data blocks and its metadata in one transaction.
//! It used to cut every write into chunks of one JBD2 descriptor block's
//! worth of tags, because a transaction could not carry more than one
//! descriptor. Since #147 a transaction carries as many descriptors as it
//! needs, so that bound was stale: the constraint that is left is that one
//! transaction has to fit in the journal.
//!
//! - A write several descriptors long, well inside the journal, commits as
//!   ONE transaction.
//! - A write larger than the whole journal is still split, and every byte
//!   of it lands.
//! - A write whose free space is fragmented into single blocks -- one
//!   extent, one extent-tree entry per block -- still fits: the chunk's
//!   metadata reservation grows with the chunk.
//!
//! The journal's own sequence number counts the transactions: every commit
//! advances it by one.

use fs_ext4::block_io::BlockDevice;
use fs_ext4::error::Result;
use fs_ext4::fs::Filesystem;
use fs_ext4::inode::Inode;
use fs_ext4::{jbd2, mkfs};
use std::sync::{Arc, Mutex};

const BLOCK_SIZE: u32 = 4096;
const BS: usize = BLOCK_SIZE as usize;
const IMAGE_BYTES: u64 = 64 * 1024 * 1024;
/// `s_sequence`, big-endian, in the JBD2 superblock.
const SEQUENCE_AT: u64 = 0x18;
/// `s_feature_incompat`, little-endian, in the ext4 superblock.
const SB_INCOMPAT_AT: usize = 1024 + 0x60;
const INCOMPAT_EXTENTS: u32 = 0x40;
/// Tags in one 4 KiB descriptor at the widest tag and with a checksum
/// tail: (4096 - 12 - 4) / 16. The old chunk was this less eight.
const TAGS_PER_DESCRIPTOR: usize = 255;

struct MemDev {
    bytes: Mutex<Vec<u8>>,
}

impl BlockDevice for MemDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let b = self.bytes.lock().unwrap();
        let start = offset as usize;
        buf.copy_from_slice(&b[start..start + buf.len()]);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut b = self.bytes.lock().unwrap();
        let start = offset as usize;
        b[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        IMAGE_BYTES
    }
    fn is_writable(&self) -> bool {
        true
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// A 64 MiB, 4 KiB-block volume with a 1024-block (4 MiB) journal -- the
/// JBD2 minimum -- whose new files are extent-mapped. This crate's mkfs
/// journals only its ext3 flavour, so the extents feature is switched on
/// afterwards; the flavour carries no metadata checksum to restamp.
fn journaled_extent_volume() -> (Arc<MemDev>, Filesystem) {
    let dev = Arc::new(MemDev {
        bytes: Mutex::new(vec![0u8; IMAGE_BYTES as usize]),
    });
    mkfs::format_filesystem_with_flavor(
        dev.as_ref(),
        None,
        None,
        IMAGE_BYTES,
        BLOCK_SIZE,
        fs_ext4::features::FsFlavor::Ext3,
    )
    .expect("format");
    {
        let mut b = dev.bytes.lock().unwrap();
        let at = SB_INCOMPAT_AT;
        let incompat = u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) | INCOMPAT_EXTENTS;
        b[at..at + 4].copy_from_slice(&incompat.to_le_bytes());
    }
    let fs = Filesystem::mount(dev.clone() as Arc<dyn BlockDevice>).expect("mount");
    assert!(fs.journal.is_some(), "fixture: the volume has a journal");
    (dev, fs)
}

fn journal_max_len(fs: &Filesystem) -> u64 {
    let jinode = Inode::parse(&fs.read_inode_raw(fs.sb.journal_inode).unwrap()).unwrap();
    jinode.size / u64::from(BLOCK_SIZE)
}

/// The sequence number the journal will give its next transaction.
fn journal_sequence(dev: &MemDev, fs: &Filesystem) -> u32 {
    let jinode = Inode::parse(&fs.read_inode_raw(fs.sb.journal_inode).unwrap()).unwrap();
    let jsb_block = jbd2::journal_block_to_physical(fs, &jinode, 0)
        .unwrap()
        .unwrap();
    let mut seq = [0u8; 4];
    dev.read_at(jsb_block * u64::from(BLOCK_SIZE) + SEQUENCE_AT, &mut seq)
        .unwrap();
    u32::from_be_bytes(seq)
}

/// Deterministic bytes that differ block to block.
fn pattern(seed: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|j| ((j / BS + seed) as u8).wrapping_mul(31) ^ (j % 251) as u8)
        .collect()
}

fn ino_of(fs: &Filesystem, path: &str) -> u32 {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, path).unwrap()
}

fn read_all(fs: &Filesystem, path: &str) -> Vec<u8> {
    let (inode, _) = fs.read_inode_verified(ino_of(fs, path)).unwrap();
    fs_ext4::file_io::read_all(fs, &inode).unwrap()
}

/// Two MiB is 512 data blocks: two descriptors' worth, and three of the
/// old chunks. The journal holds 1024 blocks, so it is one transaction.
#[test]
fn a_write_several_descriptors_long_commits_as_one_transaction() {
    let (dev, fs) = journaled_extent_volume();
    let blocks = 2 * TAGS_PER_DESCRIPTOR + 2;
    assert!(
        (blocks as u64) * 3 / 2 < journal_max_len(&fs),
        "fixture: the write fits the journal with room to spare"
    );
    fs.apply_create("/two.bin", 0o644).unwrap();
    let payload = pattern(3, blocks * BS);

    let before = journal_sequence(&dev, &fs);
    let size = fs.apply_pwrite("/two.bin", 0, &payload).unwrap();
    let after = journal_sequence(&dev, &fs);

    assert_eq!(size, payload.len() as u64);
    assert_eq!(
        after.wrapping_sub(before),
        1,
        "a {blocks}-block write that fits the journal took {} transactions",
        after.wrapping_sub(before)
    );
    assert!(read_all(&fs, "/two.bin") == payload);
}

/// Eight MiB is 2048 data blocks, twice the whole journal: it cannot be
/// one transaction, so it is split -- into as few as the journal allows,
/// not into descriptor-sized pieces -- and all of it lands.
#[test]
fn a_write_larger_than_the_journal_is_split_and_lands() {
    let (dev, fs) = journaled_extent_volume();
    let max_len = journal_max_len(&fs);
    let blocks = 2048usize;
    assert!(
        blocks as u64 > max_len,
        "fixture: the write outgrows the journal"
    );
    fs.apply_create("/eight.bin", 0o644).unwrap();
    let payload = pattern(5, blocks * BS);

    let before = journal_sequence(&dev, &fs);
    let size = fs.apply_pwrite("/eight.bin", 0, &payload).unwrap();
    let transactions = journal_sequence(&dev, &fs).wrapping_sub(before) as u64;

    assert_eq!(size, payload.len() as u64);
    // At least ceil(2048 / 1023); at most what a chunk of half the journal
    // would need, which the old 247-block chunks (nine transactions) miss.
    assert!(
        (2..=(blocks as u64).div_ceil(max_len / 2)).contains(&transactions),
        "a {blocks}-block write over a {max_len}-block journal took {transactions} transactions"
    );
    assert!(read_all(&fs, "/eight.bin") == payload);
}

/// Free space cut into single blocks: every data block of the write is its
/// own allocation and its own extent, so the extent tree grows by a leaf
/// every few dozen blocks. A chunk sized for contiguous space would outgrow
/// the journal here; the write must still land.
#[test]
fn a_write_into_fragmented_free_space_still_fits_the_journal() {
    let (_dev, fs) = journaled_extent_volume();
    // Fill the volume, then free every other block of the filler.
    fs.apply_create("/fill.bin", 0o644).unwrap();
    let free = fs.sb.free_blocks_count as usize;
    let fill_blocks = free - 64;
    let chunk = vec![0xA5u8; 256 * BS];
    let mut off = 0usize;
    while off < fill_blocks * BS {
        let take = chunk.len().min(fill_blocks * BS - off);
        fs.apply_pwrite("/fill.bin", off as u64, &chunk[..take])
            .unwrap();
        off += take;
    }
    let fill = ino_of(&fs, "/fill.bin");
    for b in (0..fill_blocks as u64).step_by(2) {
        fs.apply_fallocate_punch_hole(fill, b * BS as u64, BS as u64)
            .unwrap();
    }

    let blocks = 1500usize;
    fs.apply_create("/frag.bin", 0o644).unwrap();
    let payload = pattern(9, blocks * BS);
    let size = fs
        .apply_pwrite("/frag.bin", 0, &payload)
        .expect("a fragmented write fits the journal");
    assert_eq!(size, payload.len() as u64);
    assert!(read_all(&fs, "/frag.bin") == payload);
}
