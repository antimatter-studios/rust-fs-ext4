//! A transaction larger than one descriptor block, judged by e2fsprogs.
//!
//! One JBD2 descriptor block tags `(block_size - 12 - tail) / tag_size`
//! data blocks: 63 at 1 KiB with CSUM_V3's 16-byte tags. A transaction
//! that touches more carries several descriptors, each followed by its own
//! data blocks and each ending in a LAST tag. Until it did, `write_file`
//! (one transaction for the whole payload) failed with "descriptor block
//! overflow" at about 63 KiB on a 1 KiB-block filesystem.
//!
//! - A replace-content write well past one descriptor lands, `e2fsck -fn`
//!   finds the filesystem clean, and `debugfs` dumps the same bytes.
//! - The same kind of transaction cut off after the journal is marked dirty
//!   is replayed by `e2fsck`, whose recovery code is the kernel's: it walks
//!   every descriptor, checks every tag and tail, and writes every block.
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

const BS: u64 = 1024;
/// Tags in one CSUM_V3 descriptor at 1 KiB: (1024 - 12 - 4) / 16.
const TAGS_PER_DESCRIPTOR: usize = 63;

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

/// A fresh 64 MiB, 1 KiB-block `metadata_csum` image whose journal
/// declares CSUM_V3, as the kernel's first mount leaves it.
fn fresh_image(tag: &str) -> String {
    let image =
        fs_ext4_test_support::temp_path!("fs_ext4_multidesc_{tag}_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .unwrap();
    let (code, log) = run(
        "mkfs.ext4",
        &["-q", "-F", "-b", "1024", "-O", "metadata_csum", &image],
    );
    assert_eq!(code, Some(0), "{log}");
    // `jo -c` sets the journal checksum feature; the empty `jc` commit is
    // replayed and cleared by `e2fsck`, so the image starts clean.
    let script = format!("{image}.cmds");
    std::fs::write(&script, "jo -c\njc\n").unwrap();
    let (code, log) = run("debugfs", &["-w", "-f", &script, &image]);
    let _ = std::fs::remove_file(&script);
    assert_eq!(code, Some(0), "{log}");
    fs_ext4_test_support::oracle("e2fsck")
        .args(["-fy", &image])
        .judged()
        .repaired("e2fsck -fy");
    let (_, log) = run("dumpe2fs", &["-h", &image]);
    assert!(log.contains("journal_checksum_v3"), "{log}");
    image
}

/// Deterministic bytes that differ block to block, so a block replayed to
/// the wrong place cannot compare equal.
fn pattern(seed: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|j| ((j / BS as usize + seed) as u8).wrapping_mul(31) ^ (j % 251) as u8)
        .collect()
}

fn read_block(image: &str, block: u64) -> Vec<u8> {
    let dev = FileDevice::open(image).unwrap();
    let mut buf = vec![0u8; BS as usize];
    dev.read_at(block * BS, &mut buf).unwrap();
    buf
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

/// 512 KiB through `write_file`'s single transaction: eight descriptors'
/// worth of data blocks at 1 KiB.
#[test]
fn a_write_past_one_descriptor_is_read_back_by_debugfs() {
    let image = fresh_image("write");
    let payload = pattern(7, 512 * 1024);
    {
        let dev = FileDevice::open_rw(&image).unwrap();
        let fs = Filesystem::mount(Arc::new(dev) as Arc<dyn BlockDevice>).unwrap();
        fs.apply_create("/big.bin", 0o644).unwrap();
        let size = fs
            .apply_replace_file_content("/big.bin", &payload)
            .expect("a write larger than one descriptor block");
        assert_eq!(size, payload.len() as u64);
    }
    fs_ext4_test_support::assert_e2fsck_clean(&image, "e2fsck -fn");

    let dumped = format!("{image}.dump");
    let _ = std::fs::remove_file(&dumped);
    let (code, log) = run(
        "debugfs",
        &["-R", &format!("dump /big.bin {dumped}"), &image],
    );
    assert_eq!(code, Some(0), "{log}");
    let got = std::fs::read(&dumped).unwrap_or_else(|e| panic!("debugfs dump: {e}: {log}"));
    assert!(got == payload, "debugfs read back different bytes");
    let _ = std::fs::remove_file(&dumped);
    let _ = std::fs::remove_file(&image);
}

/// Two full descriptors and part of a third, cut off after the journal is
/// marked dirty: `e2fsck` must walk all three and replay every block.
#[test]
fn e2fsck_replays_a_transaction_split_across_descriptors() {
    let image = fresh_image("replay");
    let count = 2 * TAGS_PER_DESCRIPTOR + 5;
    // Free blocks well past group 0's metadata and the journal.
    let targets: Vec<u64> = (0..count as u64).map(|i| 60_000 + i).collect();
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
        let mut writer = JournalWriter::open(&fs).unwrap().expect("a journal");
        let mut tx = writer.begin();
        for (i, &block) in targets.iter().enumerate() {
            tx.add_write(block, pattern(i, BS as usize)).unwrap();
        }
        // Three descriptors, the data blocks, the commit; then
        // `needs_recovery` and the journal superblock marked dirty. The
        // final-location writes are lost.
        let journal_blocks = 3 + count + 1;
        let dev = CrashDevice {
            inner: Arc::new(FileDevice::open_rw(&image).unwrap()),
            budget: journal_blocks + 2,
            writes: AtomicUsize::new(0),
        };
        writer.commit(&dev, &tx).unwrap();
    }
    for &block in &targets {
        assert_eq!(
            read_block(&image, block),
            vec![0u8; BS as usize],
            "cut too late"
        );
    }

    let log = fs_ext4_test_support::oracle("e2fsck")
        .args(["-fy", &image])
        .judged()
        .repaired("e2fsck -fy");
    for (i, &block) in targets.iter().enumerate() {
        assert!(
            read_block(&image, block) == pattern(i, BS as usize),
            "e2fsck did not replay block {block} (tag {i}): {log}"
        );
    }
    fs_ext4_test_support::assert_e2fsck_clean(&image, "e2fsck -fn");
    let _ = std::fs::remove_file(&image);
}
