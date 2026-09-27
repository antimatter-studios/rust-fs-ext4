//! Data blocks that begin with the JBD2 magic, judged by e2fsprogs (#292).
//!
//! Recovery tells journal blocks from data by their first four bytes. JBD2
//! therefore has the writer zero those bytes in the logged copy of a data
//! block that begins with the magic, and set `TAG_ESCAPED` on its tag;
//! replay puts the magic back.
//!
//! - A file block shaped like a descriptor, logged unescaped, is left in the
//!   journal after its transaction checkpoints. A later, shorter transaction
//!   cut off before its checkpoint leaves that block just past its commit,
//!   where `e2fsck`'s recovery -- the kernel's -- looks for the next
//!   transaction: it finds one, made of a file's contents, and replays it.
//!   Escaped, the scan stops at the real end of the log.
//! - An escaped block cut off before its checkpoint is replayed by `e2fsck`
//!   with its magic back and its tag checksum intact, and `debugfs` reads the
//!   file back byte for byte.
//!
//! The e2fsprogs tools run in the harness VM; a test fails when it cannot reach them.

#![cfg(unix)]

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::error::Result;
use fs_ext4::jbd2::{self, JBD2_MAGIC_NUMBER};
use fs_ext4::journal_writer::JournalWriter;
use fs_ext4::transaction::Transaction;
use fs_ext4::Filesystem;
use fs_ext4_test_support::oracle;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const BS: u64 = 4096;
/// A free block on a fresh 64 MiB image, well past group 0's metadata.
const SPARE: u64 = 9000;

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

/// A fresh 4 KiB-block `metadata_csum` image whose journal declares
/// CSUM_V3, as the kernel's first mount leaves it.
fn fresh_image(tag: &str) -> String {
    let image = fs_ext4_test_support::temp_path!("fs_ext4_escape_{tag}_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .unwrap();
    let (code, log) = run(
        "mkfs.ext4",
        &["-q", "-F", "-b", "4096", "-O", "metadata_csum", &image],
    );
    assert_eq!(code, Some(0), "{log}");
    // `jo -c` sets the journal checksum feature; the empty `jc` commit is
    // replayed and cleared by `e2fsck`, so the image starts clean.
    let script = format!("{image}.cmds");
    std::fs::write(&script, "jo -c\njc\n").unwrap();
    let (code, log) = run("debugfs", &["-w", "-f", &script, &image]);
    let _ = std::fs::remove_file(&script);
    assert_eq!(code, Some(0), "{log}");
    let (code, log) = run("e2fsck", &["-fy", &image]);
    assert!(matches!(code, Some(0 | 1)), "{log}");
    let (_, log) = run("dumpe2fs", &["-h", &image]);
    assert!(log.contains("journal_checksum_v3"), "{log}");
    image
}

/// Deterministic bytes that differ block to block.
fn pattern(seed: usize) -> Vec<u8> {
    (0..BS as usize)
        .map(|j| (seed as u8).wrapping_mul(37) ^ (j % 251) as u8)
        .collect()
}

/// `pattern(seed)` with the journal magic for its first four bytes.
fn magic_led(seed: usize) -> Vec<u8> {
    let mut b = pattern(seed);
    b[0..4].copy_from_slice(&JBD2_MAGIC_NUMBER.to_be_bytes());
    b
}

fn read_block(image: &str, block: u64) -> Vec<u8> {
    let dev = FileDevice::open(image).unwrap();
    let mut buf = vec![0u8; BS as usize];
    dev.read_at(block * BS, &mut buf).unwrap();
    buf
}

/// Create `path` holding `blocks` blocks through the crate, and return the
/// fs block behind each, as `debugfs` maps them.
fn file_of(image: &str, path: &str, blocks: usize) -> Vec<u64> {
    {
        let dev = FileDevice::open_rw(image).unwrap();
        let fs = Filesystem::mount(Arc::new(dev) as Arc<dyn BlockDevice>).unwrap();
        fs.apply_create(path, 0o644).unwrap();
        let filler: Vec<u8> = (0..blocks).flat_map(|i| pattern(100 + i)).collect();
        fs.apply_replace_file_content(path, &filler).unwrap();
    }
    let out = oracle("debugfs")
        .args(["-R", &format!("blocks {path}"), image])
        .output();
    let log = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "debugfs blocks: {log}");
    let map: Vec<u64> = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(|n| n.parse().unwrap_or_else(|e| panic!("{n}: {e}: {log}")))
        .collect();
    assert_eq!(map.len(), blocks, "{log}");
    map
}

/// `path` as `debugfs` reads it.
fn dump(image: &str, path: &str) -> Vec<u8> {
    let dumped = format!("{image}.dump");
    let _ = std::fs::remove_file(&dumped);
    let (code, log) = run("debugfs", &["-R", &format!("dump {path} {dumped}"), image]);
    assert_eq!(code, Some(0), "{log}");
    let got = std::fs::read(&dumped).unwrap_or_else(|e| panic!("debugfs dump: {e}: {log}"));
    let _ = std::fs::remove_file(&dumped);
    got
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

fn crash_after(image: &str, budget: usize) -> CrashDevice {
    CrashDevice {
        inner: Arc::new(FileDevice::open_rw(image).unwrap()),
        budget,
        writes: AtomicUsize::new(0),
    }
}

/// A file whose contents are a whole JBD2 transaction must never be
/// replayed as one.
///
/// Transaction A rewrites a five-block file and checkpoints: journal blocks
/// 1..=7 are its descriptor, the five data blocks and its commit. The third
/// to fifth file blocks are a descriptor, data block and commit for the
/// sequence after B's, tagging the file's first block. Transaction B, one
/// block, is cut off after the journal is marked dirty: blocks 1..=3. Block 4
/// -- A's copy of the fake descriptor -- is where recovery looks next.
#[test]
fn a_file_shaped_like_a_transaction_is_not_replayed_as_one() {
    let image = fresh_image("stale");
    let file = file_of(&image, "/log.bin", 5);

    let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
    let jsb = jbd2::read_superblock(&fs).unwrap().expect("a journal");
    let mut writer = JournalWriter::open(&fs).unwrap().expect("a journal");

    let mut a = writer.begin();
    let fake_seq = a.sequence.wrapping_add(2);
    let mut fake = Transaction::begin(fake_seq, BS as u32, jsb.uses_64bit(), jsb.uses_csum_v3());
    fake.add_write(file[0], vec![0xEE; BS as usize]).unwrap();
    let fake = fake.commit_for(&jsb).unwrap();
    assert_eq!(fake.len(), 3, "descriptor, data, commit");
    assert_eq!(&fake[0][0..4], &JBD2_MAGIC_NUMBER.to_be_bytes());

    let contents = vec![
        pattern(1),
        pattern(2),
        fake[0].clone(),
        fake[1].clone(),
        fake[2].clone(),
    ];
    for (&block, bytes) in file.iter().zip(&contents) {
        a.add_write(block, bytes.clone()).unwrap();
    }
    writer.commit(&crash_after(&image, usize::MAX), &a).unwrap();

    let mut b = writer.begin();
    b.add_write(SPARE, pattern(9)).unwrap();
    // Descriptor, data, commit; `needs_recovery`; the dirty journal
    // superblock. The final-location write is lost.
    writer.commit(&crash_after(&image, 3 + 2), &b).unwrap();
    drop(fs);
    assert_eq!(
        read_block(&image, SPARE),
        vec![0u8; BS as usize],
        "cut too late"
    );

    // This crate's replay, on a copy, first.
    let copy = format!("{image}.crate");
    std::fs::copy(&image, &copy).unwrap();
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&copy).unwrap())).unwrap();
        fs_ext4::journal_apply::replay_if_dirty(&fs).unwrap();
    }
    let crate_first = read_block(&copy, file[0]);
    let crate_spare = read_block(&copy, SPARE);
    let _ = std::fs::remove_file(&copy);

    let (code, log) = run("e2fsck", &["-fy", &image]);
    assert!(matches!(code, Some(0 | 1)), "{log}");
    assert!(
        read_block(&image, SPARE) == pattern(9),
        "e2fsck did not replay the real transaction: {log}"
    );
    assert!(
        dump(&image, "/log.bin") == contents.concat(),
        "e2fsck replayed the file's own contents as a transaction: {log}"
    );
    let (code, log) = run("e2fsck", &["-fn", &image]);
    assert_eq!(code, Some(0), "{log}");

    assert!(crate_spare == pattern(9), "this crate did not replay B");
    assert!(
        crate_first == contents[0],
        "this crate replayed the file's own contents as a transaction"
    );
    let _ = std::fs::remove_file(&image);
}

/// An escaped block cut off before its checkpoint: `e2fsck` verifies its
/// tag checksum over the logged copy, restores the magic, and `debugfs`
/// reads the file back byte for byte.
#[test]
fn e2fsck_replays_an_escaped_block_with_its_magic() {
    let image = fresh_image("replay");
    let file = file_of(&image, "/magic.bin", 2);
    let contents = [magic_led(3), pattern(4)];
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
        let mut writer = JournalWriter::open(&fs).unwrap().expect("a journal");
        let mut tx = writer.begin();
        for (&block, bytes) in file.iter().zip(&contents) {
            tx.add_write(block, bytes.clone()).unwrap();
        }
        // Descriptor, two data, commit; then `needs_recovery` and the
        // dirty journal superblock.
        writer.commit(&crash_after(&image, 4 + 2), &tx).unwrap();
    }
    assert!(read_block(&image, file[0]) != contents[0], "cut too late");

    let (code, log) = run("e2fsck", &["-fy", &image]);
    assert!(matches!(code, Some(0 | 1)), "{log}");
    assert!(
        dump(&image, "/magic.bin") == contents.concat(),
        "e2fsck did not replay the file byte for byte: {log}"
    );
    let (code, log) = run("e2fsck", &["-fn", &image]);
    assert_eq!(code, Some(0), "{log}");
    let _ = std::fs::remove_file(&image);
}
