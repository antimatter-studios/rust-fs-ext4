//! A journal inode that maps a block onto the filesystem's own first
//! blocks is refused when the writer opens, before any commit (#183).
//!
//! `JournalWriter::open` stored every mapping as found, and the commit
//! path's bounds check refuses only a block past the filesystem or device:
//! block 0 passed, so the first commit's descriptor would land on the
//! primary superblock. The image is a fresh `mkfs.ext4` (no metadata_csum,
//! so no inode checksum to restamp) with the journal inode's second extent
//! moved, so the journal's own superblock at logical block 0 stays
//! readable; fails when the harness VM the e2fsprogs tools run in is unreachable.

use fs_ext4::block_io::FileDevice;
use fs_ext4::error::Error;
use fs_ext4::journal_writer::JournalWriter;
use fs_ext4::{bgd, Filesystem};
use std::sync::Arc;

/// A fresh image with the journal inode's second extent starting at
/// `start`, or untouched for `None`.
fn image_with_journal_at(name: &str, start: Option<u64>) -> std::path::PathBuf {
    let dir =
        fs_ext4_test_support::temp_dir().join(format!("ext4-jblock-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("j.img");
    std::fs::File::create(&img)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let made = fs_ext4_test_support::oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-O", "^metadata_csum"])
        .arg(&img)
        .output();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let Some(start) = start else {
        return img;
    };
    let (block, offset, bs) = {
        let fs =
            Filesystem::mount(Arc::new(FileDevice::open(img.to_str().unwrap()).unwrap())).unwrap();
        let ino = fs.sb.journal_inode;
        assert_eq!(ino, 8, "fixture: the journal is inode 8");
        let (block, offset) = bgd::locate_inode(&fs.sb, &fs.groups, ino).unwrap();
        (block, offset as u64, u64::from(fs.sb.block_size()))
    };
    let mut bytes = fs_ext4_test_support::read_small_file(&img).unwrap();
    // i_block at 0x28: a 12-byte extent header (magic, entries, max,
    // depth), then extents of ee_block(4) ee_len(2) ee_start_hi(2)
    // ee_start_lo(4). mkfs lays the journal out as several extents; the
    // SECOND one is moved to `start`, so logical block 0 -- the journal's
    // own superblock -- stays readable and a later logical block maps
    // onto the filesystem's first blocks: the shape the issue names.
    let header = (block * bs + offset) as usize + 0x28;
    let le16 = |b: &[u8], at: usize| u16::from_le_bytes(b[at..at + 2].try_into().unwrap());
    assert_eq!(
        le16(&bytes, header),
        0xF30A,
        "fixture: the journal inode is extent-mapped"
    );
    assert!(
        le16(&bytes, header + 2) >= 2,
        "fixture: the journal has a second extent"
    );
    assert_eq!(
        le16(&bytes, header + 6),
        0,
        "fixture: the extent tree has depth 0"
    );
    let second = header + 24;
    assert!(
        u32::from_le_bytes(bytes[second..second + 4].try_into().unwrap()) > 0,
        "fixture: the second extent starts past logical block 0"
    );
    bytes[second + 6..second + 8].copy_from_slice(&((start >> 32) as u16).to_le_bytes());
    bytes[second + 8..second + 12].copy_from_slice(&(start as u32).to_le_bytes());
    std::fs::write(&img, &bytes).unwrap();
    img
}

fn open_writer(img: &std::path::Path) -> Result<bool, Error> {
    let fs = Filesystem::mount(Arc::new(
        FileDevice::open_rw(img.to_str().unwrap()).unwrap(),
    ))?;
    JournalWriter::open(&fs).map(|w| w.is_some())
}

#[test]
fn a_journal_block_on_the_superblock_or_its_descriptor_table_is_refused_at_open() {
    for start in [0u64, 1] {
        let img = image_with_journal_at(&format!("at{start}"), Some(start));
        match open_writer(&img) {
            Err(Error::Corrupt(m)) => assert!(
                m.contains("journal inode maps a block"),
                "start {start}: {m}"
            ),
            other => panic!("a journal mapped onto block {start} opened a writer: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(img.parent().unwrap());
    }
}

/// The two edges of the refusal, on the geometry mkfs writes: the last
/// reserved-GDT block of group 0 is refused and the block after it is
/// not; an extent that ends one block past the filesystem is refused and
/// one that ends at its last block is not.
#[test]
fn the_refusal_ends_exactly_at_group_zeros_metadata_and_the_filesystems_end() {
    let img = image_with_journal_at("geometry", None);
    let (metadata_end, blocks_count, second_len) = {
        let fs =
            Filesystem::mount(Arc::new(FileDevice::open(img.to_str().unwrap()).unwrap())).unwrap();
        // The second extent's length, so an extent can be placed to end
        // exactly one block past the filesystem.
        let (block, offset) = bgd::locate_inode(&fs.sb, &fs.groups, 8).unwrap();
        let mut raw = vec![0u8; 12];
        let at = block * u64::from(fs.sb.block_size()) + offset as u64 + 0x28 + 24;
        fs_ext4::block_io::BlockDevice::read_at(fs.dev.as_ref(), at, &mut raw).unwrap();
        let second_len = u64::from(u16::from_le_bytes([raw[4], raw[5]]));
        let bs = u64::from(fs.sb.block_size());
        let descriptors = fs.sb.block_group_count() * u64::from(fs.sb.desc_size);
        assert!(
            fs.sb.reserved_gdt_blocks > 0,
            "fixture: mkfs reserves GDT growth"
        );
        (
            u64::from(fs.sb.first_data_block)
                + 1
                + descriptors.div_ceil(bs)
                + u64::from(fs.sb.reserved_gdt_blocks),
            fs.sb.blocks_count,
            second_len,
        )
    };
    assert!(second_len >= 2, "fixture: the second extent spans blocks");
    let _ = std::fs::remove_dir_all(img.parent().unwrap());

    for (name, start, refused) in [
        ("last-reserved-gdt", metadata_end - 1, true),
        ("first-after-metadata", metadata_end, false),
        // Its last block is `blocks_count`, one past the end, and no
        // further.
        ("one-past-the-end", blocks_count - second_len + 1, true),
        ("ending-at-the-end", blocks_count - second_len, false),
    ] {
        let img = image_with_journal_at(name, Some(start));
        match (open_writer(&img), refused) {
            (Err(Error::Corrupt(m)), true) => {
                assert!(m.contains("journal inode maps a block"), "{name}: {m}")
            }
            (Ok(true), false) => {}
            (other, _) => panic!("{name} (block {start}): {other:?}"),
        }
        let _ = std::fs::remove_dir_all(img.parent().unwrap());
    }
}

/// Control: the journal mkfs placed opens a writer.
#[test]
fn the_journal_mkfs_placed_opens_a_writer() {
    let img = image_with_journal_at("control", None);
    assert!(open_writer(&img).expect("the untouched journal opens"));
    let _ = std::fs::remove_dir_all(img.parent().unwrap());
}
