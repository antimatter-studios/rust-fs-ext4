//! A directory grown to a depth-2 extent tree puts every tree node on its
//! own block (#164).
//!
//! `extend_dir_and_add_entry_deep` plans every block it needs before it
//! commits any of them, and the planner reads the bitmap off the device,
//! so each plan has to be told which blocks the earlier ones took
//! (`alloc::reserved_blocks`). Without that, the plan that promotes the
//! tree to depth 2 -- a new index node and a new leaf -- gets the same
//! block twice, and two extent-tree nodes share it.
//!
//! No fixture has a directory that deep, so this builds one through the
//! driver: a 1 KiB-block volume, where an extent leaf holds 84 entries,
//! and a directory whose blocks are kept from merging into one extent by
//! a one-block file allocated between each pair of them. The inline root
//! holds four index entries, so the tree reaches depth 2 at about 340
//! directory blocks.

use fs_ext4::block_io::BlockDevice;
use fs_ext4::error::Result;
use fs_ext4::extent::{self, ExtentHeader};
use fs_ext4::fs::Filesystem;
use fs_ext4::mkfs;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

struct MemDev {
    bytes: Mutex<Vec<u8>>,
    size: u64,
}

impl BlockDevice for MemDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let b = self.bytes.lock().unwrap();
        let start = offset as usize;
        buf.copy_from_slice(&b[start..start + buf.len()]);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.size
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut b = self.bytes.lock().unwrap();
        let start = offset as usize;
        b[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

/// Every extent-tree node below the inline root, by physical block, with
/// the depth it sits at.
fn tree_nodes(node: &[u8], dev: &dyn BlockDevice, bs: u32, out: &mut Vec<(u64, u16)>) {
    let header = ExtentHeader::parse(node).expect("extent header");
    if header.depth == 0 {
        return;
    }
    for i in 0..header.entries as usize {
        let at = 12 + 12 * i;
        let lo = u32::from_le_bytes(node[at + 4..at + 8].try_into().unwrap());
        let hi = u16::from_le_bytes(node[at + 8..at + 10].try_into().unwrap());
        let child = u64::from(lo) | (u64::from(hi) << 32);
        out.push((child, header.depth - 1));
        let mut block = vec![0u8; bs as usize];
        dev.read_at(child * u64::from(bs), &mut block).unwrap();
        tree_nodes(&block, dev, bs, out);
    }
}

#[test]
fn a_directory_promoted_to_depth_two_keeps_its_tree_nodes_on_distinct_blocks() {
    let size: u64 = 8 * 1024 * 1024;
    let dev = Arc::new(MemDev {
        bytes: Mutex::new(vec![0u8; size as usize]),
        size,
    });
    mkfs::format_filesystem(dev.as_ref(), Some("DEEPDIR"), Some([0x64; 16]), size, 1024)
        .expect("format");
    let dyn_dev: Arc<dyn BlockDevice> = dev.clone();
    let fs = Filesystem::mount(dyn_dev.clone()).expect("mount");
    let bs = fs.sb.block_size();
    fs.apply_mkdir("/d", 0o755).expect("mkdir");

    let lookup = |fs: &Filesystem, path: &str| {
        fs_ext4::path::lookup(
            dyn_dev.as_ref(),
            &fs.sb,
            &mut |i| fs.read_inode_verified(i).map(|(x, _)| x),
            path,
        )
    };
    let dir_inode = |fs: &Filesystem| {
        let ino = lookup(fs, "/d").expect("look up /d");
        fs.read_inode_verified(ino).expect("read /d").0
    };
    // Long names fill a 1 KiB block in three entries.
    let name = |i: usize| format!("/d/{i:05}_{}", "n".repeat(200));
    let mut depth = 0;
    let mut i = 0;
    while depth < 2 {
        assert!(i < 3000, "the directory never reached depth 2 ({depth})");
        fs.apply_create(&name(i), 0o644)
            .unwrap_or_else(|e| panic!("create {i}: {e}"));
        fs.apply_pwrite(&name(i), 0, b"x")
            .unwrap_or_else(|e| panic!("pwrite {i}: {e}"));
        depth = ExtentHeader::parse(&dir_inode(&fs).block).unwrap().depth;
        i += 1;
    }
    // A few more entries after the promotion, through the depth-2 path.
    for j in i..i + 30 {
        fs.apply_create(&name(j), 0o644)
            .expect("create after promotion");
        fs.apply_pwrite(&name(j), 0, b"x")
            .expect("pwrite after promotion");
    }

    let inode = dir_inode(&fs);
    let mut nodes = Vec::new();
    tree_nodes(&inode.block, dev.as_ref(), bs, &mut nodes);
    assert!(
        nodes.iter().any(|&(_, d)| d == 1) && nodes.iter().filter(|&&(_, d)| d == 0).count() >= 5,
        "fixture: a depth-2 tree with an index node and several leaves, got {nodes:?}"
    );
    let mut owners: HashMap<u64, usize> = HashMap::new();
    for &(block, _) in &nodes {
        *owners.entry(block).or_default() += 1;
    }
    let shared: Vec<_> = owners.iter().filter(|&(_, &n)| n > 1).collect();
    assert!(
        shared.is_empty(),
        "extent-tree nodes share blocks {shared:?}: {nodes:?}"
    );
    let data = extent::collect_all(&inode.block, dev.as_ref(), bs).expect("collect extents");
    for e in &data {
        for &(block, _) in &nodes {
            assert!(
                !(e.physical_block..e.physical_block + u64::from(e.length)).contains(&block),
                "tree node {block} is also directory data in {e:?}"
            );
        }
    }
    // Every name is still found through the tree.
    for j in [0, i / 2, i - 1, i + 29] {
        lookup(&fs, &name(j)).unwrap_or_else(|e| panic!("lookup {j} after promotion: {e}"));
    }

    let report = fs_ext4::fsck::audit(&fs, u32::MAX, u32::MAX).expect("audit");
    assert!(report.is_clean(), "audit: {:?}", report.anomalies);
    drop(fs);
    // And e2fsck, in the harness VM.
    let image = fs_ext4_test_support::temp_path!("fs_ext4_deep_dir_{}.img", std::process::id());
    std::fs::write(&image, &*dev.bytes.lock().unwrap()).unwrap();
    fs_ext4_test_support::assert_e2fsck_clean(&image, "deep directory extent nodes");
    let _ = std::fs::remove_file(&image);
}
