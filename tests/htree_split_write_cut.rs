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

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::error::Result;
use fs_ext4::fs::Filesystem;
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

/// A fresh journaled image whose `/bigdir` holds 600 names, indexed, with
/// every leaf packed full.
fn indexed_volume(tag: &str) -> String {
    let root = fs_ext4_test_support::temp_path!("fs_ext4_split_cut_{tag}_{}", std::process::id());
    let bigdir = std::path::Path::new(&root).join("bigdir");
    std::fs::create_dir_all(&bigdir).unwrap();
    for i in 0..600 {
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
    fs_ext4_test_support::oracle("e2fsck")
        .args(["-fyD", &image])
        .judged()
        .repaired("e2fsck -fyD");
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

fn root_count(fs: &Filesystem) -> u16 {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(inode, _)| inode);
    let ino = fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, "/bigdir").unwrap();
    let (inode, _) = fs.read_inode_verified(ino).unwrap();
    let root = fs
        .read_block(fs.map_inode_logical(&inode, 0).unwrap().unwrap())
        .unwrap();
    assert_eq!(root[30], 0, "fixture: expected a one-level index");
    u16::from_le_bytes([root[34], root[35]])
}

/// Create `name` on a copy of `fixture` with writes cut after the first
/// `cut` of the create's own (`usize::MAX`: none cut). Returns the copy,
/// the number of writes the create issued, and whether it split a leaf.
fn create_with_cut(fixture: &str, name: &str, cut: usize, tag: &str) -> (String, usize, bool) {
    let image = format!("{fixture}.{tag}");
    std::fs::copy(fixture, &image).unwrap();
    let device = Arc::new(CutDevice {
        inner: FileDevice::open_rw(&image).unwrap(),
        budget: AtomicUsize::new(usize::MAX),
        writes: AtomicUsize::new(0),
    });
    let fs = Filesystem::mount(device.clone()).expect("mount");
    let before = root_count(&fs);
    let start = device.writes.load(Ordering::SeqCst);
    device
        .budget
        .store(start.saturating_add(cut), Ordering::SeqCst);
    let created = fs.apply_create(name, 0o644);
    let issued = device.writes.load(Ordering::SeqCst) - start;
    let split = cut == usize::MAX && {
        created.expect("create");
        root_count(&fs) > before
    };
    drop(fs);
    (image, issued, split)
}

#[test]
fn every_write_cut_inside_an_htree_leaf_split_leaves_a_clean_volume() {
    let fixture = indexed_volume("csum");
    // `e2fsck -D` leaves some slack in a leaf, so names go in, uncut,
    // until one splits a leaf: that create is the one cut.
    let (name, writes) = (0..800)
        .find_map(|i| {
            let name = format!("/bigdir/a_longer_name_to_fill_leaves_{i:05}");
            let (image, writes, split) = create_with_cut(&fixture, &name, usize::MAX, "probe");
            if split {
                let _ = std::fs::remove_file(image);
                Some((name, writes))
            } else {
                std::fs::rename(image, &fixture).unwrap();
                None
            }
        })
        .expect("fixture: 800 creates split no leaf");
    assert!(writes > 0, "the split issued no writes");

    let mut rejected = Vec::new();
    for cut in 0..=writes {
        let (image, _, _) = create_with_cut(&fixture, &name, cut, &format!("cut{cut}"));
        // The remount replays whatever the journal committed.
        drop(Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).expect("remount"));
        // The verdict, not the exit status: `e2fsck -n` exits 0 having
        // answered "no" to a damaged index. A report that is not a verdict
        // is a rejection too, never a pass.
        let judged = fs_ext4_test_support::oracle("e2fsck")
            .args(["-fn", &image])
            .judged();
        match judged.verdict {
            fs_ext4_test_support::Verdict::Clean => {}
            fs_ext4_test_support::Verdict::Findings(report) => {
                rejected.push(format!("--- cut after write {cut} of {writes}:\n{report}"));
            }
            fs_ext4_test_support::Verdict::NotAVerdict(why) => {
                rejected.push(format!(
                    "--- cut after write {cut} of {writes}: not a verdict: {why}"
                ));
            }
        }
        let _ = std::fs::remove_file(image);
    }
    let _ = std::fs::remove_file(&fixture);
    assert!(
        rejected.is_empty(),
        "e2fsck rejected {} of {} cut points:\n{}",
        rejected.len(),
        writes + 1,
        rejected.join("\n")
    );
}
