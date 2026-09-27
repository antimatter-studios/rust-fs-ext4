//! A read-only mount of a dirty journal reads the committed state, as
//! `e2fsck` recovers it, and writes nothing (#72).
//!
//! A crate `mkdir` is cut off after its journal is marked dirty: the
//! transaction is committed in the log and none of its final-location
//! writes landed. A read-only mount used to skip replay and report the
//! directory absent and the counters as they were. It now replays into the
//! buffer cache. The reference is the same image after `e2fsck -fy`, which
//! recovers the journal with the kernel's code.
//!
//! The e2fsprogs tools run in the harness VM; a test fails when it cannot reach them.

#![cfg(unix)]

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::error::Result;
use fs_ext4::Filesystem;
use fs_ext4_test_support::oracle;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

fn run(tool: &str, args: &[&str]) -> (Option<i32>, String) {
    let out = fs_ext4_test_support::oracle(tool).args(args).output();
    (
        out.status.code(),
        format!(
            "{tool} {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// Once armed, keeps writes only until the second flush: a commit's journal
/// blocks (flush one) and its dirty journal superblock (flush two). Then the
/// power goes.
struct CutAfterDirtyJournal {
    inner: Arc<dyn BlockDevice>,
    armed: AtomicBool,
    flushes: AtomicUsize,
}

impl BlockDevice for CutAfterDirtyJournal {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.armed.load(Ordering::SeqCst) && self.flushes.load(Ordering::SeqCst) >= 2 {
            return Ok(());
        }
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> Result<()> {
        if self.armed.load(Ordering::SeqCst) {
            self.flushes.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.flush()
    }
    fn is_writable(&self) -> bool {
        true
    }
}

fn names(fs: &Filesystem, dir: &str) -> Vec<Vec<u8>> {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    let ino = fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, dir).expect("lookup");
    let (inode, _) = fs.read_inode_verified(ino).unwrap();
    let data = fs_ext4::file_io::read_all(fs, &inode).unwrap();
    let bs = fs.sb.block_size() as usize;
    let mut out: Vec<Vec<u8>> = data
        .chunks(bs)
        .flat_map(|b| fs_ext4::dir::parse_block(b, true).unwrap_or_default())
        .map(|e| e.name)
        .collect();
    out.sort();
    out
}

/// A fresh image whose journal holds a committed `mkdir /committed` that
/// never reached its final location: replay is the only way to see it.
fn image_with_committed_mkdir(tag: &str) -> String {
    let mkfs = "mkfs.ext4";
    let e2fsck = "e2fsck";
    let debugfs = "debugfs";
    let image =
        fs_ext4_test_support::temp_path!("fs_ext4_ro_replay_{tag}_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .unwrap();
    let (code, log) = run(mkfs, &["-q", "-F", "-b", "4096", &image]);
    assert_eq!(code, Some(0), "{log}");
    // A CSUM_V3 journal, as a kernel mount leaves it.
    let script = format!("{image}.cmds");
    std::fs::write(&script, "jo -c\njc\n").unwrap();
    let (code, log) = run(debugfs, &["-w", "-f", &script, &image]);
    let _ = std::fs::remove_file(&script);
    assert_eq!(code, Some(0), "{log}");
    oracle(e2fsck)
        .args(["-fy", &image])
        .judged()
        .repaired("e2fsck -fy after the debugfs script");

    {
        let dev = Arc::new(CutAfterDirtyJournal {
            inner: Arc::new(FileDevice::open_rw(&image).unwrap()),
            armed: AtomicBool::new(false),
            flushes: AtomicUsize::new(0),
        });
        let fs = Filesystem::mount(dev.clone()).expect("mount rw");
        assert!(
            fs.journal.is_some(),
            "the mkdir must go through the journal"
        );
        // The first write of a mount marks the volume not clean, with a
        // flush of its own (#85). Made here, before the cut is armed, so the
        // flushes the cut counts are the mkdir's commit and nothing else.
        fs.apply_mkdir("/warmup", 0o755).expect("warm-up mkdir");
        dev.armed.store(true, Ordering::SeqCst);
        fs.apply_mkdir("/committed", 0o755).expect("mkdir");
    }

    // The cut left the mkdir only in the journal: a writable lazy mount,
    // which defers replay, reads the data area as it is.
    {
        let on_disk =
            Filesystem::mount_lazy(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
        assert!(
            !names(&on_disk, "/").contains(&b"committed".to_vec()),
            "the cut came too late: the mkdir reached its final location"
        );
    }
    image
}

#[test]
fn a_read_only_mount_reads_what_the_journal_committed() {
    let e2fsck = "e2fsck";
    let image = image_with_committed_mkdir("read");

    let before = std::fs::read(&image).unwrap();
    let ro = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).expect("mount ro");
    let ro_root = names(&ro, "/");
    assert!(
        ro_root.contains(&b"committed".to_vec()),
        "a read-only mount skipped the committed mkdir: {:?}",
        ro_root
            .iter()
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .collect::<Vec<_>>()
    );
    let ro_counts = (
        ro.groups[0].free_blocks_count,
        ro.groups[0].free_inodes_count,
        ro.groups[0].used_dirs_count,
    );
    drop(ro);
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a read-only mount wrote to the device"
    );

    // The reference: the kernel's recovery code on a copy.
    let recovered = format!("{image}.recovered");
    std::fs::copy(&image, &recovered).unwrap();
    oracle(e2fsck)
        .args(["-fy", &recovered])
        .judged()
        .repaired("the reference recovery");
    fs_ext4_test_support::assert_e2fsck_clean(&recovered, "the recovered reference");
    let reference = Filesystem::mount(Arc::new(FileDevice::open(&recovered).unwrap())).unwrap();
    assert_eq!(ro_root, names(&reference, "/"));
    let ro = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
    assert_eq!(names(&ro, "/committed"), names(&reference, "/committed"));
    drop(ro);
    // Group 0's descriptor is journaled with the mkdir, and mount read the
    // descriptor table before replay.
    assert_eq!(
        ro_counts,
        (
            reference.groups[0].free_blocks_count,
            reference.groups[0].free_inodes_count,
            reference.groups[0].used_dirs_count,
        ),
        "group 0's counters after in-memory replay"
    );
    let _ = std::fs::remove_file(&image);
    let _ = std::fs::remove_file(&recovered);
}

/// The root directory's names as `debugfs` reads them, `.` and `..` included.
fn debugfs_root_names(image: &str) -> Vec<Vec<u8>> {
    let out = fs_ext4_test_support::oracle("debugfs")
        .args(["-R", "ls -p /", image])
        .output();
    assert_eq!(
        out.status.code(),
        Some(0),
        "debugfs ls: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // `ls -p` prints `/ino/mode/uid/gid/name/size/` per entry.
    let mut names: Vec<Vec<u8>> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().strip_prefix('/'))
        .filter_map(|l| l.split('/').nth(4).map(|n| n.as_bytes().to_vec()))
        .collect();
    names.sort();
    names
}

/// `fresh_read` on a read-only mount must not throw away what replay put in
/// the cache (#298). The replayed blocks exist only in memory: the device
/// still holds the pre-replay bytes, and nothing was checkpointed, so a
/// cache discard can only turn committed state back into superseded state.
/// The expected view is e2fsprogs' own recovery of a copy, read by debugfs.
#[test]
fn fresh_read_keeps_replayed_blocks_a_read_only_mount_cannot_checkpoint() {
    let image = image_with_committed_mkdir("fresh");
    let recovered = format!("{image}.recovered");
    std::fs::copy(&image, &recovered).unwrap();
    let (code, log) = run("e2fsck", &["-fy", &recovered]);
    assert!(matches!(code, Some(0 | 1)), "{log}");
    let expected = debugfs_root_names(&recovered);
    assert!(
        expected.contains(&b"committed".to_vec()),
        "the oracle's recovery lost the committed mkdir: {expected:?}"
    );

    let mut ro = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).expect("mount ro");
    assert_eq!(
        names(&ro, "/"),
        expected,
        "the replayed view before fresh_read"
    );
    let fresh = ro.fresh_read();
    let after = names(&ro, "/");
    assert_eq!(
        after,
        expected,
        "fresh_read ({fresh:?}) discarded the replayed blocks and read the pre-replay device: {:?}",
        after
            .iter()
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .collect::<Vec<_>>()
    );
    assert!(
        fresh.is_err(),
        "fresh_read claimed a physical readback the device cannot give"
    );
    drop(ro);
    let _ = std::fs::remove_file(&image);
    let _ = std::fs::remove_file(&recovered);
}
