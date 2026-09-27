//! Writing into an htree-indexed directory leaves an index e2fsck accepts.
//!
//! The write path treated every directory block as a linear one. Block 0 of
//! an indexed directory is the `dx_root`, whose fake `..` record spans the
//! rest of the block, so the first create split that record and wrote the new
//! entry over `dx_root_info` and the whole `dx_entry` array (#97). Lookups
//! here scan linearly when the index misses, so nothing in this crate
//! noticed. e2fsck does.
//!
//! The volume comes from the real toolchain: `mkfs.ext4 -d` copies in a
//! directory of names, and `e2fsck -fyD` indexes it. The driver then creates,
//! links, renames into and unlinks from that directory, and `e2fsck -fn` must
//! find nothing, with and without metadata_csum. Fails without e2fsprogs
//! (they run in the harness VM).

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use fs_ext4::inode::InodeFlags;
use std::sync::Arc;

fn run(tool: &str, args: &[&str]) -> (Option<i32>, String) {
    let out = fs_ext4_test_support::oracle(tool).args(args).output();
    (
        out.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// A fresh image whose `/bigdir` holds `count` files and is indexed.
fn indexed_volume(tag: &str, features: &str, count: usize) -> String {
    let root = fs_ext4_test_support::temp_path!("fs_ext4_htree_w_{tag}_{}", std::process::id());
    let bigdir = std::path::Path::new(&root).join("bigdir");
    std::fs::create_dir_all(bigdir.join("sub")).unwrap();
    for i in 0..count {
        std::fs::write(bigdir.join(format!("existing_file_{i:05}")), b"").unwrap();
    }
    std::fs::create_dir_all(std::path::Path::new(&root).join("elsewhere")).unwrap();
    std::fs::write(std::path::Path::new(&root).join("elsewhere/mover"), b"m").unwrap();
    // An indexed directory that is itself moved, so its dx_root's `..`
    // changes.
    let bigsub = std::path::Path::new(&root).join("elsewhere/bigsub");
    std::fs::create_dir_all(&bigsub).unwrap();
    for i in 0..count {
        std::fs::write(bigsub.join(format!("inner_{i:05}")), b"").unwrap();
    }
    let image = format!("{root}.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .unwrap();
    let (code, log) = run(
        "mkfs.ext4",
        &[
            "-q", "-F", "-b", "1024", "-O", features, "-d", &root, &image,
        ],
    );
    assert_eq!(code, Some(0), "mkfs.ext4: {log}");
    fs_ext4_test_support::oracle("e2fsck")
        .args(["-fyD", &image])
        .judged()
        .repaired("e2fsck -fyD indexes the fixture");
    let _ = std::fs::remove_dir_all(&root);
    image
}

fn resolve(fs: &Filesystem, path: &str) -> fs_ext4::Result<u32> {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(inode, _)| inode);
    fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, path)
}

fn is_indexed(fs: &Filesystem, path: &str) -> bool {
    let ino = resolve(fs, path).expect("resolve");
    let (inode, _) = fs.read_inode_verified(ino).expect("inode");
    inode.flags & InodeFlags::INDEX.bits() != 0
}

/// `-n` answers "no" and can still exit 0 with IGNORED lines, so the
/// report is read rather than the exit status; one that is not a verdict
/// at all fails here.
fn e2fsck_clean(image: &str) -> Result<(), String> {
    let judged = fs_ext4_test_support::oracle("e2fsck")
        .args(["-fn", image])
        .judged();
    match judged.verdict {
        fs_ext4_test_support::Verdict::Clean => Ok(()),
        fs_ext4_test_support::Verdict::Findings(report) => Err(report),
        fs_ext4_test_support::Verdict::NotAVerdict(why) => {
            panic!("{}: not a verdict on {image}: {why}", judged.call)
        }
    }
}

fn exercise(tag: &str, features: &str) {
    let image = indexed_volume(tag, features, 600);
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
        assert!(
            is_indexed(&fs, "/bigdir"),
            "[{tag}] fixture: e2fsck -D did not index /bigdir"
        );
        for i in 0..40 {
            fs.apply_create(&format!("/bigdir/new_{i}"), 0o644)
                .expect("create");
        }
        fs.apply_link("/bigdir/new_0", "/bigdir/linked")
            .expect("link");
        fs.apply_rename("/elsewhere/mover", "/bigdir/moved_in", false)
            .expect("rename in");
        fs.apply_unlink("/bigdir/existing_file_00007")
            .expect("unlink");
        fs.apply_mkdir("/bigdir/newdir", 0o755).expect("mkdir");
        assert!(
            is_indexed(&fs, "/elsewhere/bigsub"),
            "[{tag}] fixture: bigsub not indexed"
        );
        fs.apply_rename("/elsewhere/bigsub", "/bigdir/bigsub", false)
            .expect("rename an indexed directory");
        for i in 0..40 {
            assert!(
                resolve(&fs, &format!("/bigdir/new_{i}")).is_ok(),
                "[{tag}] new_{i} not found"
            );
        }
    }
    if let Err(report) = e2fsck_clean(&image) {
        panic!("[{tag}] e2fsck after writes into an indexed directory:\n{report}");
    }
    let _ = std::fs::remove_file(&image);
}

#[test]
fn writes_into_an_indexed_directory_without_metadata_csum() {
    exercise("nocsum", "^metadata_csum,^has_journal");
}

#[test]
fn writes_into_an_indexed_directory_with_metadata_csum() {
    exercise("csum", "metadata_csum");
}

/// A full leaf splits, and the directory stays indexed (#195).
///
/// `e2fsck -D` packs the leaves full, so the first create into any of them
/// splits it: half its names move to a new block, the name goes into its
/// half, and the root gains the entry routing the new block. 800 creates
/// into a 600-name directory split many times without filling the 1 KiB
/// root, which holds 123 routes under metadata_csum -- 1500 filled it on
/// x86_64, where the names hash to a different spread. Every name must then
/// be in the leaf the index routes it to -- a lookup here falls back to a
/// linear scan, so finding it is not enough -- and e2fsck, which checks the
/// whole index, must be clean.
fn split_leaves(tag: &str, features: &str) {
    let image = indexed_volume(tag, features, 600);
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
        let root_count = |fs: &Filesystem| {
            let ino = resolve(fs, "/bigdir").unwrap();
            let (inode, _) = fs.read_inode_verified(ino).unwrap();
            let root = fs
                .read_block(fs.map_inode_logical(&inode, 0).unwrap().unwrap())
                .unwrap();
            assert_eq!(root[30], 0, "[{tag}] fixture: expected a one-level index");
            u16::from_le_bytes([root[34], root[35]])
        };
        let before = root_count(&fs);
        let mut names: Vec<String> = (0..600).map(|i| format!("existing_file_{i:05}")).collect();
        for i in 0..800 {
            let name = format!("a_longer_name_to_fill_leaves_{i:05}");
            fs.apply_create(&format!("/bigdir/{name}"), 0o644)
                .expect("create");
            names.push(name);
        }
        assert!(is_indexed(&fs, "/bigdir"), "[{tag}] the index was dropped");
        let after = root_count(&fs);
        assert!(
            after > before,
            "[{tag}] no leaf split: root count {before} -> {after}"
        );

        let ino = resolve(&fs, "/bigdir").unwrap();
        let (inode, _) = fs.read_inode_verified(ino).unwrap();
        let block = |logical: u64| {
            fs.read_block(fs.map_inode_logical(&inode, logical).unwrap().unwrap())
                .unwrap()
        };
        let root = block(0);
        for name in &names {
            let leaf = fs_ext4::htree::lookup_leaf_with(
                name.as_bytes(),
                &root,
                &fs.sb.hash_seed,
                fs.sb.unsigned_hash(),
                |logical| Ok(block(u64::from(logical))),
            )
            .unwrap()
            .unwrap();
            let held = fs_ext4::dir::parse_block(&block(u64::from(leaf)), true)
                .unwrap()
                .iter()
                .any(|e| e.name == name.as_bytes());
            assert!(
                held,
                "[{tag}] {name} is not in leaf {leaf}, where the index routes it"
            );
        }
    }
    if let Err(report) = e2fsck_clean(&image) {
        panic!("[{tag}] e2fsck after leaf splits:\n{report}");
    }
    let _ = std::fs::remove_file(&image);
}

#[test]
fn a_full_leaf_splits_without_metadata_csum() {
    split_leaves("split_nocsum", "^metadata_csum,^has_journal");
}

#[test]
fn a_full_leaf_splits_with_metadata_csum() {
    split_leaves("split_csum", "metadata_csum");
}

/// When the index block routing a full leaf has no room for another entry,
/// the index is dropped and the directory carries on as a linear one, as the
/// kernel's `dx_fallback` does. Splitting an interior node is not done here.
/// A block appended to a still-indexed directory is one no lookup through
/// the index would reach.
///
/// 6000 names at 1 KiB blocks need a second index level, and `e2fsck -D`
/// packs its nodes full, so the drop also converts interior nodes. With
/// metadata_csum each converted block needs a dirent tail, and e2fsck checks
/// every one.
fn fill_a_leaf(tag: &str, features: &str) {
    let image = indexed_volume(tag, features, 6000);
    let mut created = Vec::new();
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
        let bigdir = resolve(&fs, "/bigdir").unwrap();
        let (inode, _) = fs.read_inode_verified(bigdir).unwrap();
        let root_phys = fs.map_inode_logical(&inode, 0).unwrap().unwrap();
        let root = fs.read_block(root_phys).unwrap();
        assert_eq!(root[30], 1, "[{tag}] fixture: expected a two-level index");

        for i in 0..3000 {
            let path = format!("/bigdir/a_longer_name_to_fill_leaves_{i:05}");
            fs.apply_create(&path, 0o644).expect("create");
            created.push(path);
            if !is_indexed(&fs, "/bigdir") {
                break;
            }
        }
        assert!(
            !is_indexed(&fs, "/bigdir"),
            "[{tag}] 3000 creates never filled an index node; the fallback went untested"
        );
        // A few more after the drop, through the linear path.
        for i in 0..20 {
            let path = format!("/bigdir/after_the_drop_{i}");
            fs.apply_create(&path, 0o644).expect("create after drop");
            created.push(path);
        }
        for path in created
            .iter()
            .chain([&"/bigdir/existing_file_05999".to_string()])
        {
            assert!(resolve(&fs, path).is_ok(), "[{tag}] {path} not found");
        }
    }
    if let Err(report) = e2fsck_clean(&image) {
        panic!("[{tag}] e2fsck after an index was dropped:\n{report}");
    }
    let _ = std::fs::remove_file(&image);
}

#[test]
fn a_full_leaf_drops_the_index_without_metadata_csum() {
    fill_a_leaf("fill_nocsum", "^metadata_csum,^has_journal");
}

#[test]
fn a_full_leaf_drops_the_index_with_metadata_csum() {
    fill_a_leaf("fill_csum", "metadata_csum");
}

/// A root whose `info_length` is not 8 is refused before it routes a write
/// (CodeRabbit on #196): its count and limit would be read from the wrong
/// offset. Tested without metadata_csum, where no checksum would catch it.
#[test]
fn a_root_with_a_wrong_info_length_is_refused() {
    let image = indexed_volume("info_len", "^metadata_csum,^has_journal", 600);
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
        let ino = resolve(&fs, "/bigdir").expect("resolve");
        let (inode, _) = fs.read_inode_verified(ino).expect("inode");
        let phys = fs.map_inode_logical(&inode, 0).unwrap().unwrap();
        let bs = u64::from(fs.sb.block_size());
        let mut root = fs.read_block(phys).unwrap();
        root[29] = 16;
        fs.dev.write_at(phys * bs, &root).unwrap();
        fs.dev.flush().unwrap();
    }
    let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
    match fs.apply_create("/bigdir/through_a_bad_root", 0o644) {
        Err(fs_ext4::Error::Corrupt(m)) => assert!(m.contains("info_length"), "{m}"),
        other => panic!("a create went through a bad root: {:?}", other.map(|_| ())),
    }
    let _ = std::fs::remove_file(&image);
}

/// Flip one byte of `path`'s dx_root `dx_tail` checksum, on the device.
fn corrupt_dx_root_checksum(image: &str, path: &str) {
    let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(image).unwrap())).unwrap();
    let ino = resolve(&fs, path).expect("resolve");
    let (inode, _) = fs.read_inode_verified(ino).expect("inode");
    let phys = fs.map_inode_logical(&inode, 0).unwrap().unwrap();
    let bs = u64::from(fs.sb.block_size());
    let mut root = fs.read_block(phys).unwrap();
    let count_offset = 24 + usize::from(root[29]);
    let limit = usize::from(u16::from_le_bytes([
        root[count_offset],
        root[count_offset + 1],
    ]));
    let checksum_at = count_offset + limit * 8 + 4;
    root[checksum_at] ^= 0xFF;
    fs.dev.write_at(phys * bs, &root).unwrap();
    fs.dev.flush().unwrap();
}

/// An index block whose checksum is wrong is not used to route a write or
/// restamped over (Greptile on #196): a create into the directory, and a
/// move of the directory, are both refused with a checksum error, and the
/// damage is left for e2fsck to see rather than hidden under a fresh
/// checksum.
#[test]
fn a_corrupt_index_root_is_neither_routed_through_nor_restamped() {
    let image = indexed_volume("corrupt_root", "metadata_csum", 600);
    corrupt_dx_root_checksum(&image, "/bigdir");
    corrupt_dx_root_checksum(&image, "/elsewhere/bigsub");
    let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
    match fs.apply_create("/bigdir/routed_by_a_corrupt_root", 0o644) {
        Err(fs_ext4::Error::BadChecksum { .. }) => {}
        other => panic!(
            "a create was routed through a corrupt root: {:?}",
            other.map(|_| ())
        ),
    }
    match fs.apply_rename("/elsewhere/bigsub", "/bigdir2", false) {
        Err(fs_ext4::Error::BadChecksum { .. }) => {}
        other => panic!(
            "a corrupt root was restamped by a move: {:?}",
            other.map(|_| ())
        ),
    }
    drop(fs);
    let report = fs_ext4_test_support::oracle("e2fsck")
        .args(["-fn", &image])
        .judged()
        .findings("a corrupt index root");
    assert!(
        report.contains("HTREE") || report.contains("checksum"),
        "the corruption must still be visible to e2fsck:\n{report}"
    );
    let _ = std::fs::remove_file(&image);
}
