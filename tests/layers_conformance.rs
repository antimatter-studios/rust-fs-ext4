//! rust-fs-core's salvage conformance set (`fs_core::layers::conformance::
//! salvage`) run against [`Ext4Adapter`], and the strict API it is judged
//! beside: [`Ext4Volume::open`] and the file layer reading every file.
//!
//! The images are volumes this crate's `mkfs` makes and its write path
//! fills, in memory: a healthy one holding a deleted inode the way the
//! kernel leaves one (no links, `i_dtime` set, checksum valid), and damaged
//! copies, each with the evidence the set requires it to show:
//!
//! - a primary superblock with a wrong magic number: a `Signature` finding;
//! - a file's inode with a broken checksum: a candidate whose integrity
//!   failed;
//! - a fragmented file's extent leaf zeroed: a walk that ends early;
//! - a directory's block zeroed: a `Malformed` finding;
//! - an inode table block that cannot be read: an `Unreadable` finding.
//!
//! No tools and no VM: the kernel-made images are judged by
//! `tests/layers_oracle.rs`.

use fs_core::layers::conformance::salvage::{run, Damaged, Driver, Evidence};
use fs_core::layers::{
    DamageKind, Extent, FileAPI, NodeKind, OpenVolume, SalvageAdapter, VolumeAPI,
};
use fs_core::BlockRead;
use fs_ext4::block_io::BlockDevice;
use fs_ext4::checksum::Checksummer;
use fs_ext4::features::FsFlavor;
use fs_ext4::layers::{Ext4Adapter, Ext4Volume, LayerError};
use fs_ext4::mkfs::format_filesystem_with_flavor;
use fs_ext4::salvage;
use std::sync::{Arc, Mutex};

/// A whole volume in memory, readable as a source and writable for the
/// crate's `mkfs` and write path.
struct Memory(Mutex<Vec<u8>>);

impl BlockDevice for Memory {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_ext4::Result<()> {
        let bytes = self.0.lock().unwrap();
        buf.copy_from_slice(&bytes[offset as usize..offset as usize + buf.len()]);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_ext4::Result<()> {
        let mut bytes = self.0.lock().unwrap();
        bytes[offset as usize..offset as usize + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.0.lock().unwrap().len() as u64
    }
    fn is_writable(&self) -> bool {
        true
    }
}

/// An owned copy of a source, for the strict API, which takes its source by
/// value.
struct Owned(Vec<u8>);

impl BlockRead for Owned {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .filter(|&end| end <= self.0.len() as u64)
            .ok_or(fs_core::Error::OutOfBounds {
                offset,
                len: buf.len() as u64,
                size: self.0.len() as u64,
            })?;
        buf.copy_from_slice(&self.0[offset as usize..end as usize]);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.0.len() as u64
    }
}

fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(37) ^ seed)
        .collect()
}

/// The healthy image, and the inode numbers the damaged cases aim at.
struct Built {
    image: Vec<u8>,
    file: u32,
    fragmented: u32,
    dir: u32,
}

/// 8 MiB of 1 KiB-block ext4 (one group, metadata_csum): a few files, a
/// directory, a symlink, a fragmented file, and a deleted inode.
fn build() -> Built {
    let memory = Arc::new(Memory(Mutex::new(vec![0; 8 << 20])));
    format_filesystem_with_flavor(
        &*memory,
        Some("layers"),
        Some([3; 16]),
        8 << 20,
        1024,
        FsFlavor::Ext4,
    )
    .unwrap();
    let (file, fragmented, dir, doomed) = {
        let fs = fs_ext4::Filesystem::mount(memory.clone()).unwrap();
        let file = fs.apply_create("/file", 0o644).unwrap();
        fs.apply_pwrite("/file", 0, &pattern(1, 3000)).unwrap();
        let dir = fs.apply_mkdir("/dir", 0o755).unwrap();
        for i in 0..4 {
            let path = format!("/dir/f{i}");
            fs.apply_create(&path, 0o644).unwrap();
            fs.apply_pwrite(&path, 0, &pattern(i as u8, 700 * (i + 1)))
                .unwrap();
        }
        fs.apply_symlink("dir/f0", "/link").unwrap();
        let fragmented = fs.apply_create("/fragmented", 0o644).unwrap();
        fs.apply_create("/pad", 0o644).unwrap();
        for i in 0..8u64 {
            fs.apply_pwrite("/fragmented", i * 1024, &pattern(i as u8, 1024))
                .unwrap();
            fs.apply_pwrite("/pad", i * 1024, &pattern(0xAA, 1024))
                .unwrap();
        }
        let doomed = fs.apply_create("/doomed", 0o644).unwrap();
        // Deleted, its map kept: what an older kernel or another writer
        // leaves, and what deleted-extent recovery is for.
        fs.apply_pwrite("/doomed", 0, &pattern(9, 2500)).unwrap();
        (file, fragmented, dir, doomed)
    };
    let at = inode_at(&memory.0.lock().unwrap(), doomed);
    let kept = memory.0.lock().unwrap()[at.clone()].to_vec();
    fs_ext4::Filesystem::mount(memory.clone())
        .unwrap()
        .apply_unlink("/doomed")
        .unwrap();
    let mut image = memory.0.lock().unwrap().clone();
    image[at].copy_from_slice(&kept);
    delete_like_the_kernel(&mut image, doomed);
    Built {
        image,
        file,
        fragmented,
        dir,
    }
}

/// Where inode `ino`'s bytes are.
fn inode_at(image: &[u8], ino: u32) -> std::ops::Range<usize> {
    let source = Owned(image.to_vec());
    let sb = salvage::find_superblock(&source)
        .unwrap()
        .superblock
        .unwrap();
    let descriptors = salvage::group_descriptors(&source, &sb, 0);
    let record = salvage::read_inode(&source, &sb, &descriptors, ino).unwrap();
    let start = record.location.offset as usize;
    start..start + record.location.length as usize
}

/// The kernel deletes an inode by removing its entry and setting `i_dtime`
/// with no links left, keeping its mode (`tests/salvage_verbs_oracle.rs`).
/// The crate's unlink clears the slot instead, so the caller puts the
/// inode's bytes back and this marks them deleted, checksum recomputed.
fn delete_like_the_kernel(image: &mut [u8], ino: u32) {
    let at = inode_at(image, ino);
    let mut raw = image[at.clone()].to_vec();
    raw[0x1A..0x1C].copy_from_slice(&0u16.to_le_bytes()); // i_links_count
    raw[0x14..0x18].copy_from_slice(&1_700_000_000u32.to_le_bytes()); // i_dtime
    let source = Owned(image.to_vec());
    let sb = salvage::find_superblock(&source)
        .unwrap()
        .superblock
        .unwrap();
    let generation = u32::from_le_bytes(raw[0x64..0x68].try_into().unwrap());
    if let Some((lo, hi)) =
        Checksummer::from_superblock(&sb).compute_inode_checksum(ino, generation, &raw)
    {
        raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
        raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
    }
    image[at].copy_from_slice(&raw);
}

struct Ext4;

impl Driver for Ext4 {
    type Adapter = Ext4Adapter;
    type StrictError = LayerError;

    fn salvage(&self, source: &dyn BlockRead) -> Ext4Adapter {
        let sb = salvage::find_superblock(source)
            .ok()
            .and_then(|copy| copy.superblock)
            .or_else(|| {
                salvage::find_superblock(&Owned(build().image))
                    .ok()?
                    .superblock
            })
            .unwrap();
        Ext4Adapter::new(sb)
    }

    fn strict(&self, source: &dyn BlockRead) -> Result<(), LayerError> {
        let mut bytes = vec![0u8; source.size_bytes() as usize];
        source.read_at(0, &mut bytes)?;
        let volume = Ext4Volume::open(Owned(bytes))?;
        if !volume.health()?.is_clean() {
            return Err(LayerError::Ext4(fs_ext4::Error::Corrupt(
                "the volume reports damage",
            )));
        }
        let mut pending = vec![volume.root()?];
        while let Some(dir) = pending.pop() {
            for entry in volume.read_dir(&dir)? {
                match entry.kind {
                    NodeKind::Directory => pending.push(entry.node),
                    NodeKind::File => {
                        volume.copy_to(&entry.node, &mut std::io::sink())?;
                    }
                    _ => {
                        volume.metadata(&entry.node)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn healthy(&self) -> Vec<u8> {
        build().image
    }

    fn damaged(&self) -> Vec<Damaged> {
        let built = build();
        let mut cases = Vec::new();

        let mut image = built.image.clone();
        image[1024 + 0x38..1024 + 0x3A].copy_from_slice(&[0, 0]);
        cases.push(Damaged::new(
            "primary superblock magic",
            image,
            Evidence::Damage(DamageKind::Signature),
        ));

        let mut image = built.image.clone();
        let at = inode_at(&image, built.file);
        image[at.start + 0x20] ^= 0xFF; // i_ctime
        cases.push(Damaged::new(
            "inode checksum",
            image,
            Evidence::FailedCandidate,
        ));

        let source = Owned(built.image.clone());
        let adapter = Ext4.salvage(&source);
        let leaf = {
            let sb = adapter.native().superblock().clone();
            let descriptors = salvage::group_descriptors(&source, &sb, 0);
            let record = salvage::read_inode(&source, &sb, &descriptors, built.fragmented).unwrap();
            let map = salvage::map_inode(&source, &sb, &record);
            assert!(
                !map.map_blocks.is_empty(),
                "the fragmented file has no leaf"
            );
            map.map_blocks[0] as usize * 1024
        };
        let mut image = built.image.clone();
        image[leaf..leaf + 1024].fill(0);
        cases.push(Damaged::new(
            "extent leaf zeroed",
            image,
            Evidence::BrokenWalk,
        ));

        let dir_block = {
            let sb = adapter.native().superblock().clone();
            let descriptors = salvage::group_descriptors(&source, &sb, 0);
            let record = salvage::read_inode(&source, &sb, &descriptors, built.dir).unwrap();
            salvage::read_directory(&source, &sb, &record).blocks[0]
                .1
                .offset as usize
        };
        let mut image = built.image.clone();
        image[dir_block..dir_block + 1024].fill(0);
        cases.push(Damaged::new(
            "directory block zeroed",
            image,
            Evidence::Damage(DamageKind::Malformed),
        ));

        let table = inode_at(&built.image, built.file).start as u64 / 1024 * 1024;
        cases.push(
            Damaged::new(
                "inode table block unreadable",
                built.image.clone(),
                Evidence::Damage(DamageKind::Unreadable),
            )
            .unreadable(Extent::new(table, 1024)),
        );
        cases
    }
}

#[test]
fn the_salvage_adapter_conforms() {
    run(&Ext4).assert_conforms();
}

#[test]
fn the_adapter_lacks_nothing() {
    let image = build().image;
    let adapter = Ext4.salvage(&Owned(image));
    assert!(adapter.capabilities().missing.is_empty());
}

/// Geometry hypotheses: every superblock copy that parses, sound ones
/// first, each building an adapter that agrees with the volume's own.
#[test]
fn every_parseable_superblock_copy_is_a_hypothesis() {
    use fs_core::layers::PrimitiveAPI;
    // Two groups of 2 KiB blocks, so a backup exists in group 1.
    let memory = Arc::new(Memory(Mutex::new(vec![0; 64 << 20])));
    format_filesystem_with_flavor(
        &*memory,
        None,
        Some([4; 16]),
        64 << 20,
        2048,
        FsFlavor::Ext4,
    )
    .unwrap();
    let mut image = memory.0.lock().unwrap().clone();
    image[1024 + 0x78] ^= 0xFF; // the primary's label: its checksum fails
    let volume = Ext4Volume::open_for_salvage(Owned(image)).unwrap();
    let hypotheses = volume.hypotheses().unwrap();
    assert_eq!(hypotheses.len(), 2, "{hypotheses:?}");
    assert!(
        hypotheses[0].sound && hypotheses[0].group == 1,
        "{hypotheses:?}"
    );
    assert!(
        !hypotheses[1].sound && hypotheses[1].group == 0,
        "{hypotheses:?}"
    );
    for hypothesis in &hypotheses {
        let salvage = volume.salvage_with(hypothesis).unwrap();
        assert!(!salvage.all_candidates().unwrap().is_empty());
    }
}
