//! Every write path that allocates blocks lands them in a `BLOCK_UNINIT`
//! group without handing one out twice (#291).
//!
//! Staging an allocation into a `BLOCK_UNINIT` group clears the flag on the
//! transaction's buffer only; the mount sees it when the buffer commits. A
//! second plan in the same transaction that is not told about the buffer's
//! pending clears still sees the flag, rebuilds the group's bitmap from its
//! metadata alone and offers the block it has just staged. #145 fixed that
//! for `pwrite`. A punch that splits an extent in a tree already packed full
//! needs two fresh blocks — a fifth leaf, and an index node above the five —
//! and planned both that way: the leaf and the index node were written to one
//! block, and the index named itself as its own child.
//!
//! The scenario is the one where that matters. `mkfs.ext4` leaves a group it
//! put nothing into `BLOCK_UNINIT`. Every group before the first such one is
//! filled, so any allocation hinted at group 0 goes to a group that is still
//! uninit. Each write path is then run once, on its own copy of that volume,
//! and must take some group's flag down (it really did allocate there) and
//! leave a volume `e2fsck -fn` accepts. The paths that plan only once per transaction, or commit between
//! plans, are run the same way so the property stays pinned if that changes.
//! `mkfs.ext4` and `e2fsck` judge it, from the harness VM they live in.

use fs_ext4::bgd::BgdFlags;
use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use fs_ext4_test_support::oracle;
use std::sync::Arc;

const BS: u64 = 4096;
/// 32 MiB groups, so a 256 MiB volume has eight and filling one is quick.
const BLOCKS_PER_GROUP: &str = "8192";
/// A three-block run every four blocks: each its own extent record.
const STRIDE: u64 = 4 * BS;
/// Extent records a 4 KiB node holds: (4096 - 12) / 12.
const NODE_CAP: u64 = 340;
/// One more than four full leaves, so the last one can be punched away and
/// the rest repacked into exactly four.
const STRIPES: u64 = 4 * NODE_CAP + 1;

fn image(tag: &str) -> String {
    fs_ext4_test_support::temp_path!("fs_ext4_291_{tag}_{}.img", std::process::id())
}

fn mkfs(path: &str, features: Option<&str>) {
    std::fs::File::create(path)
        .and_then(|f| f.set_len(256 * 1024 * 1024))
        .unwrap();
    let mut mkfs = oracle("mkfs.ext4").args([
        "-q",
        "-F",
        "-b",
        "4096",
        "-g",
        BLOCKS_PER_GROUP,
        "-N",
        "4096",
        "-I",
        "256",
    ]);
    if let Some(features) = features {
        mkfs = mkfs.args(["-O", features]);
    }
    let out = mkfs.arg(path).output();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn e2fsck_clean(path: &str, what: &str) {
    let out = oracle("e2fsck").args(["-fn", path]).output();
    assert!(
        out.status.success(),
        "[{what}] e2fsck -fn rejected the volume:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

fn mount(path: &str) -> Filesystem {
    Filesystem::mount(Arc::new(FileDevice::open_rw(path).unwrap())).unwrap()
}

fn block_uninit(fs: &Filesystem, gi: usize) -> bool {
    fs.groups[gi].flags().contains(BgdFlags::BLOCK_UNINIT)
}

/// The first group `mkfs.ext4` left `BLOCK_UNINIT`. Groups holding a backup
/// superblock are initialised, so it is not necessarily group 1.
fn first_uninit(fs: &Filesystem) -> usize {
    (1..fs.groups.len())
        .find(|&gi| block_uninit(fs, gi))
        .expect("mkfs.ext4 left no group uninit, so this tests nothing")
}

/// Fill every group before `target`, one file per free run, lowest run
/// first. The allocator is first-fit from the inode's group (0), so a file
/// sized to the lowest free run lands exactly on it, in one piece, and
/// nothing spills into `target`.
fn fill_groups_before(path: &str, target: usize) {
    for k in 0..256 {
        let fs = mount(path);
        let Some(gi) = (0..target).find(|&gi| fs.groups[gi].free_blocks_count > 0) else {
            assert!(block_uninit(&fs, target), "filling touched group {target}");
            return;
        };
        let bitmap = fs.read_block(fs.groups[gi].block_bitmap).unwrap();
        let bits = fs.sb.blocks_per_group as usize;
        let used = |bit: usize| bitmap[bit / 8] & (1 << (bit % 8)) != 0;
        let start = (0..bits).find(|&b| !used(b)).expect("a free bit");
        let len = (start..bits).take_while(|&b| !used(b)).count() as u64;
        let name = format!("/fill-{k}");
        fs.apply_create(&name, 0o644).expect("create filler");
        if fs.flavor.uses_extents() {
            fs.apply_pwrite(&name, 0, &vec![0xF1; (len * BS) as usize])
                .expect("write filler");
        } else {
            // A block-mapped file is replaced in one transaction, so it is
            // kept well inside the 16 MiB journal. It takes its indirect
            // blocks from the same run, so the data is the run less those.
            // Where no size fills it exactly, the block left over is the
            // next run.
            let take = len.min(2048);
            let data = (1..=take as u32)
                .rev()
                .find(|&d| {
                    u64::from(d) + fs_ext4::indirect_mut::count_indirect_blocks(d, BS as u32)
                        <= take
                })
                .expect("one data block fits any run");
            fs.apply_replace_file_content(&name, &vec![0xF1; data as usize * BS as usize])
                .expect("write filler");
        }
    }
    panic!("the groups before {target} did not fill in 256 runs");
}

fn fill(i: u64) -> u8 {
    (i % 251 + 1) as u8
}

/// The volume each case starts from: a striped file whose extent tree is four
/// full leaves under an inline root, a root directory of four extents, and
/// every group before the first uninit one full. Built once; each case takes
/// a copy.
///
/// The root directory rather than a new one, because a new directory is
/// placed away from its parent — perhaps in the uninit group, which would
/// then no longer be — while the root's blocks are hinted at group 0.
fn base(tag: &str) -> (String, u32) {
    let path = image(tag);
    mkfs(&path, None);
    let file = {
        let fs = mount(&path);
        grow_root_to_four_extents(&fs);
        let file = fs.apply_create("/striped", 0o644).expect("create");
        for i in 0..STRIPES {
            fs.apply_pwrite("/striped", i * STRIDE, &vec![fill(i); 3 * BS as usize])
                .unwrap_or_else(|e| panic!("write stripe {i}: {e:?}"));
        }
        // Punching the last stripe away leaves exactly four leaves' worth,
        // and the repack packs them full.
        fs.apply_fallocate_punch_hole(file, (STRIPES - 1) * STRIDE, 3 * BS)
            .expect("punch the last stripe");
        file
    };
    {
        let fs = mount(&path);
        assert_eq!(
            root_shape(&fs, file),
            (1, 4),
            "the striped file is not four leaves under the root"
        );
        assert_eq!(
            root_shape(&fs, ROOT),
            (0, 4),
            "the root directory is not four extents in the inode"
        );
    }
    let target = first_uninit(&mount(&path));
    fill_groups_before(&path, target);
    e2fsck_clean(&path, "the base volume");
    (path, file)
}

const ROOT: u32 = 2;

/// `(depth, entries)` of an inode's extent root.
fn root_shape(fs: &Filesystem, ino: u32) -> (u16, u16) {
    let (inode, _) = fs.read_inode_verified(ino).unwrap();
    let field = |at: usize| u16::from_le_bytes(inode.block[at..at + 2].try_into().unwrap());
    (field(6), field(2))
}

/// Grow the root directory to four blocks, each its own extent: a one-block
/// file written between growths keeps each new block apart from the last.
fn grow_root_to_four_extents(fs: &Filesystem) {
    for spacer in 0..3 {
        grow_dir(fs, &format!("s{spacer}"));
        let name = format!("/spacer-{spacer}");
        fs.apply_create(&name, 0o644).unwrap();
        fs.apply_pwrite(&name, 0, &[0x5A; BS as usize]).unwrap();
    }
}

/// Add names to the root directory until it grows by one block. Long names,
/// so a block holds few of them and few inodes are spent.
fn grow_dir(fs: &Filesystem, tag: &str) {
    let before = fs.read_inode_verified(ROOT).unwrap().0.size;
    for n in 0.. {
        let name = format!("/{tag}-{n:04}-{}", "n".repeat(180));
        fs.apply_create(&name, 0o644)
            .expect("create in the root directory");
        if fs.read_inode_verified(ROOT).unwrap().0.size > before {
            return;
        }
    }
}

fn uninit_groups(fs: &Filesystem) -> Vec<usize> {
    (0..fs.groups.len())
        .filter(|&gi| block_uninit(fs, gi))
        .collect()
}

/// Run `op` on a copy of `base`, and require that it allocated out of a group
/// that was uninit and left a volume e2fsck accepts.
fn case(base: &str, tag: &str, op: impl FnOnce(&Filesystem)) {
    let path = image(tag);
    std::fs::copy(base, &path).unwrap();
    let before = {
        let fs = mount(&path);
        let before = uninit_groups(&fs);
        assert!(!before.is_empty(), "[{tag}] no group is uninit");
        op(&fs);
        before
    };
    let after = uninit_groups(&mount(&path));
    assert!(
        before.iter().any(|gi| !after.contains(gi)),
        "[{tag}] nothing was allocated from an uninit group, so this tests nothing"
    );
    e2fsck_clean(&path, tag);
    let _ = std::fs::remove_file(&path);
}

/// The defective path. The split adds one record to a tree of four full
/// leaves: a fifth leaf, and an index node above five.
#[test]
fn a_splitting_punch_needs_two_blocks_from_an_uninit_group() {
    let (base, file) = base("punch-base");
    let path = image("punch");
    std::fs::copy(&base, &path).unwrap();
    let uninit_before = {
        let fs = mount(&path);
        let before = uninit_groups(&fs).len();
        fs.apply_fallocate_punch_hole(file, BS, BS)
            .expect("punch the middle of the first stripe");
        before
    };
    e2fsck_clean(&path, "after a punch that needed two blocks");

    let fs = mount(&path);
    assert!(
        uninit_groups(&fs).len() < uninit_before,
        "the punch did not allocate from an uninit group"
    );
    let (inode, _) = fs.read_inode_verified(file).unwrap();
    let depth = u16::from_le_bytes(inode.block[6..8].try_into().unwrap());
    assert_eq!(depth, 2, "the repack did not add an index level");
    let (extents, nodes) =
        fs_ext4::extent::collect_all_with_nodes(&inode.block, fs.dev.as_ref(), BS as u32).unwrap();
    assert_eq!(extents.len() as u64, STRIPES);
    let mut distinct = nodes.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        nodes.len(),
        "two tree nodes share a block: {nodes:?}"
    );
    let mut buf = vec![0u8; BS as usize];
    for (block, want) in [(0u64, fill(0)), (1, 0), (2, fill(0))] {
        fs_ext4::file_io::read(&fs, &inode, block * BS, BS, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == want), "stripe 0 block {block}");
    }
    let last = STRIPES - 2;
    fs_ext4::file_io::read(&fs, &inode, last * STRIDE, BS, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == fill(last)), "the last stripe");
    drop(fs);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&base);
}

/// The paths that plan once per transaction, or commit between plans.
#[test]
fn every_other_extent_mapped_path_allocates_from_an_uninit_group_once() {
    let (base, _) = base("paths-base");

    case(&base, "fallocate", |fs| {
        let ino = fs.apply_create("/prealloc", 0o644).unwrap();
        fs.apply_fallocate_keep_size(ino, 0, 8 * BS).unwrap();
    });
    case(&base, "setxattr", |fs| {
        fs.apply_create("/attrs", 0o644).unwrap();
        fs.apply_setxattr("/attrs", "user.big", &[0xA5; 2000])
            .unwrap();
    });
    case(&base, "symlink", |fs| {
        fs.apply_symlink(&"t".repeat(200), "/slow-link").unwrap();
    });
    case(&base, "replace", |fs| {
        fs.apply_create("/replaced", 0o644).unwrap();
        fs.apply_replace_file_content("/replaced", &vec![0x3C; 5 * BS as usize])
            .unwrap();
    });
    case(&base, "mkdir", |fs| {
        // Orlov places it away from the root; its block goes to its group
        // or, that being full, to the next one with room.
        fs.apply_mkdir("/made", 0o755).unwrap();
    });
    // The fifth extent promotes the directory's root to a leaf block: the
    // data block is committed first, then the leaf block is planned.
    case(&base, "dir-promote", |fs| {
        grow_dir(fs, "p");
        assert_eq!(
            root_shape(fs, ROOT).0,
            1,
            "the directory's root was not promoted"
        );
        // And once more, through the depth-1 path.
        grow_dir(fs, "q");
    });
    let _ = std::fs::remove_file(&base);
}

/// The block-mapped paths, on an ext4 volume without extents: still
/// checksummed, so its groups still start out uninit.
#[test]
fn the_block_mapped_paths_allocate_from_an_uninit_group_once() {
    let path = image("mapped-base");
    mkfs(&path, Some("^extent,^64bit"));
    let target = {
        let fs = mount(&path);
        assert!(
            !fs.flavor.uses_extents(),
            "the volume maps blocks by extents"
        );
        first_uninit(&fs)
    };
    fill_groups_before(&path, target);
    e2fsck_clean(&path, "the block-mapped base volume");

    case(&path, "mapped-replace", |fs| {
        fs.apply_create("/replaced", 0o644).unwrap();
        // Past the twelve direct blocks, so the run carries an indirect block.
        fs.apply_replace_file_content("/replaced", &vec![0x3C; 20 * BS as usize])
            .unwrap();
    });
    case(&path, "mapped-dir", |fs| grow_dir(fs, "m"));
    let _ = std::fs::remove_file(&path);
}
