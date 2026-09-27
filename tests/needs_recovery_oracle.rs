//! `needs_recovery` brackets a live journal, judged by e2fsprogs (#228).
//!
//! Linux decides whether to replay from the ext4 superblock's
//! `INCOMPAT_RECOVER`, not from the journal's `s_start`: a filesystem
//! without it has its journal wiped at mount. `e2fsck` shares that recovery
//! code and says so ("Superblock needs_recovery flag is clear, but journal
//! has data") before asking whether to run the journal anyway.
//!
//! - A crate commit cut after the journal is marked dirty leaves the flag
//!   set, and `e2fsck` recovers without asking.
//! - A cut in the middle of writing a transaction that journals the
//!   superblock block itself still leaves the flag set.
//! - A finished commit, and a replay by this crate, leave neither the flag
//!   nor a dirty journal behind.
//!
//! The e2fsprogs tools run in the harness VM; a test fails when it cannot reach them.

#![cfg(unix)]

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::error::Result;
use fs_ext4::journal_writer::JournalWriter;
use fs_ext4::Filesystem;
use fs_ext4_test_support::oracle;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const BS: u64 = 4096;
/// Free blocks on a fresh 64 MiB image, well past group 0's metadata.
const TARGETS: [u64; 3] = [9000, 9001, 9002];
const RECOVER: u32 = 0x4;
const CLEAR_BUT_DATA: &str = "needs_recovery flag is clear, but journal has data";

fn run(tool: &str, args: &[&str]) -> (Option<i32>, String) {
    let out = oracle(tool).args(args).output();
    (
        out.status.code(),
        format!(
            "{tool} {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// A fresh `metadata_csum` image whose journal declares CSUM_V3, as the
/// kernel's first mount leaves it.
/// `e2fsck -fy`: replays, repairs, and must not have declined to look or
/// left anything standing. Its transcript.
fn repair(image: &str) -> String {
    oracle("e2fsck")
        .args(["-fy", image])
        .judged()
        .repaired("e2fsck -fy")
}

/// `e2fsck -fn`: must examine the volume and find nothing. Its transcript.
fn check(image: &str) -> String {
    let judged = oracle("e2fsck").args(["-fn", image]).judged();
    let log = judged.report();
    judged.clean("e2fsck -fn");
    log
}

fn fresh_image(tag: &str) -> String {
    let mkfs = "mkfs.ext4";
    let debugfs = "debugfs";
    let image =
        fs_ext4_test_support::temp_path!("fs_ext4_recover_{tag}_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .unwrap();
    let (code, log) = run(mkfs, &["-q", "-F", "-b", "4096", &image]);
    assert_eq!(code, Some(0), "{log}");
    // `jo -c` sets the checksum feature; an empty `jc` commit is replayed
    // and cleared by `e2fsck` so the image starts clean.
    let script = format!("{image}.cmds");
    std::fs::write(&script, "jo -c\njc\n").unwrap();
    let (code, log) = run(debugfs, &["-w", "-f", &script, &image]);
    let _ = std::fs::remove_file(&script);
    assert_eq!(code, Some(0), "{log}");
    repair(&image);
    assert!(
        !flag_set(&image),
        "the fixture starts without needs_recovery"
    );
    image
}

fn pattern(i: usize) -> Vec<u8> {
    (0..BS as usize)
        .map(|j| (0x40 + i as u8 * 0x10).wrapping_add((j % 251) as u8))
        .collect()
}

fn read_at(image: &str, offset: u64, len: usize) -> Vec<u8> {
    let dev = FileDevice::open(image).unwrap();
    let mut buf = vec![0u8; len];
    dev.read_at(offset, &mut buf).unwrap();
    buf
}

fn flag_set(image: &str) -> bool {
    let sb = read_at(image, 1024, 1024);
    u32::from_le_bytes(sb[0x60..0x64].try_into().unwrap()) & RECOVER != 0
}

/// Drops every write after the first `budget`: a power cut.
struct CrashDevice {
    inner: Arc<dyn BlockDevice>,
    budget: usize,
    writes: AtomicUsize,
}

impl BlockDevice for CrashDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.writes.fetch_add(1, Ordering::SeqCst) >= self.budget {
            return Ok(());
        }
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> Result<()> {
        self.inner.flush()
    }
    fn is_writable(&self) -> bool {
        self.inner.is_writable()
    }
}

/// Commit `writes` through the crate's journal writer on a device that
/// keeps only the first `budget` writes.
fn commit(image: &str, writes: &[(u64, Vec<u8>)], budget: usize) {
    let fs = Filesystem::mount(Arc::new(FileDevice::open(image).unwrap())).unwrap();
    let mut writer = JournalWriter::open(&fs).unwrap().expect("a journal");
    let mut tx = writer.begin();
    for (block, bytes) in writes {
        tx.add_write(*block, bytes.clone()).unwrap();
    }
    let dev = CrashDevice {
        inner: Arc::new(FileDevice::open_rw(image).unwrap()),
        budget,
        writes: AtomicUsize::new(0),
    };
    writer.commit(&dev, &tx).unwrap();
}

fn targets() -> Vec<(u64, Vec<u8>)> {
    TARGETS
        .iter()
        .enumerate()
        .map(|(i, &b)| (b, pattern(i)))
        .collect()
}

/// Journal writes for three blocks: descriptor, data x3, commit.
const JOURNAL_BLOCKS: usize = 5;

#[test]
fn a_crash_after_the_journal_is_dirty_leaves_the_flag_for_linux() {
    let image = fresh_image("cut");
    // The journal blocks, the flag, the dirty journal superblock.
    commit(&image, &targets(), JOURNAL_BLOCKS + 2);
    assert!(
        flag_set(&image),
        "a live journal without needs_recovery is wiped by Linux"
    );

    let log = repair(&image);
    assert!(!log.contains(CLEAR_BUT_DATA), "{log}");
    for (i, &block) in TARGETS.iter().enumerate() {
        assert!(
            read_at(&image, block * BS, BS as usize) == pattern(i),
            "e2fsck did not recover block {block}: {log}"
        );
    }
    let _ = std::fs::remove_file(&image);
}

/// A transaction journaling the superblock block carries the flag set: its
/// final-location write lands before the others, and must not clear it.
#[test]
fn a_journaled_superblock_block_keeps_the_flag() {
    let image = fresh_image("sb");
    let block0 = read_at(&image, 0, BS as usize);
    // Block 0 twice, the way a transaction touching the superblock in two
    // places can carry it: step 3 applies both, so both keep the flag.
    let writes = vec![(0, block0.clone()), (TARGETS[0], pattern(0)), (0, block0)];
    // Everything up to and including the second superblock write: descriptor,
    // three data, commit, flag, dirty journal, block 0, the target, block 0.
    commit(&image, &writes, 5 + 2 + 3);
    assert!(
        flag_set(&image),
        "step 3 wrote a superblock without needs_recovery over a live journal"
    );
    let log = repair(&image);
    assert!(!log.contains(CLEAR_BUT_DATA), "{log}");
    assert!(read_at(&image, TARGETS[0] * BS, BS as usize) == pattern(0));
    check(&image);
    let _ = std::fs::remove_file(&image);
}

#[test]
fn a_finished_commit_leaves_neither_flag_nor_journal() {
    let image = fresh_image("done");
    commit(&image, &targets(), usize::MAX);
    assert!(!flag_set(&image));
    let log = check(&image);
    assert!(!log.contains("journal"), "{log}");
    let _ = std::fs::remove_file(&image);
}

/// This crate's replay finishes the job: the writes land, the journal is
/// clean and the flag is gone, so `e2fsck` finds nothing to recover.
#[test]
fn the_crates_replay_clears_the_journal_and_the_flag() {
    let image = fresh_image("replay");
    commit(&image, &targets(), JOURNAL_BLOCKS + 2);
    assert!(flag_set(&image));

    Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).expect("mount replays");
    for (i, &block) in TARGETS.iter().enumerate() {
        assert!(read_at(&image, block * BS, BS as usize) == pattern(i));
    }
    assert!(!flag_set(&image), "replay left needs_recovery set");
    let log = check(&image);
    assert!(
        !log.contains(CLEAR_BUT_DATA),
        "replay left the journal dirty: {log}"
    );

    // And the journal the replay left is one the crate writes on again.
    commit(&image, &targets(), usize::MAX);
    check(&image);
    let _ = std::fs::remove_file(&image);
}
