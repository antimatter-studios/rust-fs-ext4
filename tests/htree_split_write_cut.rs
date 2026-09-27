//! A write cut anywhere inside an htree leaf split leaves a volume e2fsck
//! accepts (#302).
//!
//! Splitting a full leaf used to append the new right leaf outside the
//! transaction that rewrote the left leaf and routed the new one from the
//! parent index: the allocation, the extent insert, the inode's new size and
//! the leaf's bytes went to disk first, and the halved leaf and the routing
//! entry were committed afterwards. A cut between the two left the directory
//! with a mapped, allocated block its index never referenced, which e2fsck
//! reports as a damaged index -- on a journaled volume too, because the first
//! half never went through the journal.
//!
//! The volume comes from the real toolchain: `mkfs.ext4 -d` copies in a
//! directory of names and `e2fsck -fyD` indexes it, packing every leaf full,
//! so the first create into the directory splits a leaf. That create is run
//! once to count its device writes, then once per cut point `k`, with every
//! write after the first `k` dropped as a power loss drops them. Each cut
//! image is remounted, which replays the journal, and `e2fsck -fn` must find
//! nothing. Every tool runs in the harness VM.
//!
//! Where the parent index block is full the leaf cannot split, and the index
//! is dropped instead (#347). That drop used to commit what the create had
//! staged -- the new inode and its bitmap -- and then rewrite the directory's
//! inode and index blocks outside the journal, so a cut inside the drop left
//! an inode no name reached, or a half-converted index. The second test cuts
//! that create the same way.

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::error::Result;
use fs_ext4::fs::Filesystem;
use fs_ext4::inode::InodeFlags;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// A fresh journaled image whose `/bigdir` holds `names` names, indexed,
/// with every leaf and interior node packed full.
fn indexed_volume(tag: &str, names: usize) -> String {
    let root = fs_ext4_test_support::temp_path!("fs_ext4_split_cut_{tag}_{}", std::process::id());
    let bigdir = std::path::Path::new(&root).join("bigdir");
    std::fs::create_dir_all(&bigdir).unwrap();
    for i in 0..names {
        std::fs::write(bigdir.join(format!("existing_file_{i:05}")), b"").unwrap();
    }
    let image = format!("{root}.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .unwrap();
    let (code, log) = run(
        "mkfs.ext4",
        &[
            "-q",
            "-F",
            "-b",
            "1024",
            "-O",
            "metadata_csum,has_journal",
            "-d",
            &root,
            &image,
        ],
    );
    assert_eq!(code, Some(0), "mkfs.ext4: {log}");
    let (code, log) = run("e2fsck", &["-fyD", &image]);
    assert!(matches!(code, Some(0 | 1)), "e2fsck -fyD: {log}");
    let _ = std::fs::remove_dir_all(&root);
    image
}

/// Passes the first `budget` writes through and drops every later one,
/// reporting success, as a device that loses power drops what it had not
/// yet written. Reads see only what reached the file.
struct CutDevice {
    inner: FileDevice,
    budget: AtomicUsize,
    writes: AtomicUsize,
}

impl BlockDevice for CutDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let n = self.writes.fetch_add(1, Ordering::SeqCst);
        if n >= self.budget.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> Result<()> {
        self.inner.flush()
    }
    fn is_writable(&self) -> bool {
        true
    }
}

/// `/bigdir`'s index as it stands: its number of interior levels and the
/// root's entry count, or `None` once the directory is no longer indexed.
fn index_shape(fs: &Filesystem) -> Option<(u8, u16)> {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(inode, _)| inode);
    let ino = fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, "/bigdir").unwrap();
    let (inode, _) = fs.read_inode_verified(ino).unwrap();
    if inode.flags & InodeFlags::INDEX.bits() == 0 {
        return None;
    }
    let root = fs
        .read_block(fs.map_inode_logical(&inode, 0).unwrap().unwrap())
        .unwrap();
    Some((root[30], u16::from_le_bytes([root[34], root[35]])))
}

/// What an uncut create did to `/bigdir`'s index.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Effect {
    /// Nothing was measured: the create was cut.
    Cut,
    /// The index stayed as it was.
    Unchanged,
    /// A one-level root gained an entry: a leaf split.
    Split,
    /// The index was dropped.
    Dropped,
}

/// Create `name` on a copy of `fixture` with writes cut after the first
/// `cut` of the create's own (`usize::MAX`: none cut). Returns the copy,
/// the number of writes the create issued, and what it did to the index.
fn create_with_cut(fixture: &str, name: &str, cut: usize, tag: &str) -> (String, usize, Effect) {
    let image = format!("{fixture}.{tag}");
    std::fs::copy(fixture, &image).unwrap();
    let device = Arc::new(CutDevice {
        inner: FileDevice::open_rw(&image).unwrap(),
        budget: AtomicUsize::new(usize::MAX),
        writes: AtomicUsize::new(0),
    });
    let fs = Filesystem::mount(device.clone()).expect("mount");
    let before = index_shape(&fs);
    let start = device.writes.load(Ordering::SeqCst);
    device
        .budget
        .store(start.saturating_add(cut), Ordering::SeqCst);
    let created = fs.apply_create(name, 0o644);
    let issued = device.writes.load(Ordering::SeqCst) - start;
    let effect = if cut == usize::MAX {
        created.expect("create");
        match (before, index_shape(&fs)) {
            (Some(_), None) => Effect::Dropped,
            (Some((0, was)), Some((0, now))) if now > was => Effect::Split,
            _ => Effect::Unchanged,
        }
    } else {
        Effect::Cut
    };
    drop(fs);
    (image, issued, effect)
}

/// Cut the create of `name` on `fixture` after every write in turn, from
/// none to all `writes`, remount each cut, and return e2fsck's report on
/// every one it rejected.
fn rejected_cuts(fixture: &str, name: &str, writes: usize) -> Vec<String> {
    let mut rejected = Vec::new();
    for cut in 0..=writes {
        let (image, _, _) = create_with_cut(fixture, name, cut, &format!("cut{cut}"));
        // The remount replays whatever the journal committed.
        drop(Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).expect("remount"));
        let (code, report) = run("e2fsck", &["-fn", &image]);
        if code != Some(0) || report.contains("IGNORED") || report.contains("HTREE") {
            rejected.push(format!("--- cut after write {cut} of {writes}:\n{report}"));
        }
        let _ = std::fs::remove_file(image);
    }
    rejected
}

#[test]
fn every_write_cut_inside_an_htree_leaf_split_leaves_a_clean_volume() {
    let fixture = indexed_volume("csum", 600);
    // `e2fsck -D` leaves some slack in a leaf, so names go in, uncut,
    // until one splits a leaf: that create is the one cut.
    let (name, writes) = (0..800)
        .find_map(|i| {
            let name = format!("/bigdir/a_longer_name_to_fill_leaves_{i:05}");
            let (image, writes, effect) = create_with_cut(&fixture, &name, usize::MAX, "probe");
            if effect == Effect::Split {
                let _ = std::fs::remove_file(image);
                Some((name, writes))
            } else {
                assert_eq!(effect, Effect::Unchanged, "fixture: {name} did not split");
                std::fs::rename(image, &fixture).unwrap();
                None
            }
        })
        .expect("fixture: 800 creates split no leaf");
    assert!(writes > 0, "the split issued no writes");

    let rejected = rejected_cuts(&fixture, &name, writes);
    let _ = std::fs::remove_file(&fixture);
    assert!(
        rejected.is_empty(),
        "e2fsck rejected {} of {} cut points:\n{}",
        rejected.len(),
        writes + 1,
        rejected.join("\n")
    );
}

/// 6000 names at 1 KiB blocks need a second index level, and `e2fsck -D`
/// packs its interior nodes full, so a leaf that fills cannot be routed
/// from its node and the create drops the index (#347). The names before
/// that create go in uncut, in one mount; the create itself is cut.
#[test]
fn every_write_cut_inside_an_htree_index_drop_leaves_a_clean_volume() {
    let fixture = indexed_volume("drop", 6000);
    let name = |i: usize| format!("/bigdir/a_longer_name_to_fill_leaves_{i:05}");
    // mkfs picks a fresh hash seed, so the replay below starts from a copy
    // of this volume rather than a second mkfs.
    let probe = format!("{fixture}.probe");
    std::fs::copy(&fixture, &probe).unwrap();
    let drops_at = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&probe).unwrap())).unwrap();
        assert_eq!(
            index_shape(&fs).map(|(levels, _)| levels),
            Some(1),
            "fixture: expected a two-level index"
        );
        (0..3000)
            .find(|&i| {
                fs.apply_create(&name(i), 0o644).expect("create");
                index_shape(&fs).is_none()
            })
            .expect("fixture: 3000 creates never dropped the index")
    };
    let _ = std::fs::remove_file(&probe);
    // The same creates, on the untouched volume, stopping one short of the
    // drop.
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&fixture).unwrap())).unwrap();
        for i in 0..drops_at {
            fs.apply_create(&name(i), 0o644).expect("create");
        }
    }
    let (image, writes, effect) = create_with_cut(&fixture, &name(drops_at), usize::MAX, "probe");
    let _ = std::fs::remove_file(image);
    assert_eq!(
        effect,
        Effect::Dropped,
        "fixture: the create did not drop the index"
    );
    assert!(writes > 0, "the drop issued no writes");

    let rejected = rejected_cuts(&fixture, &name(drops_at), writes);
    let _ = std::fs::remove_file(&fixture);
    assert!(
        rejected.is_empty(),
        "e2fsck rejected {} of {} cut points:\n{}",
        rejected.len(),
        writes + 1,
        rejected.join("\n")
    );
}
