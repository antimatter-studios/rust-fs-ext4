//! Large `apply_pwrite`s cut to the journal, judged by e2fsprogs (#293).
//!
//! A pwrite is cut into chunks that each fit one transaction in the
//! journal, rather than one descriptor block's worth of tags. On a
//! kernel-formatted volume with a 4 MiB (1024-block) journal:
//!
//! - a 2 MiB write -- two descriptors' worth of tags -- is one transaction,
//! - a 12 MiB write -- three journals' worth -- is several,
//!
//! and after both `e2fsck -fn` finds the filesystem clean and `debugfs`
//! dumps the same bytes back.
//!
//! The e2fsprogs tools run in the harness VM; a test fails when it cannot reach them.

#![cfg(unix)]

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::inode::Inode;
use fs_ext4::jbd2;
use fs_ext4::Filesystem;
use fs_ext4_test_support::oracle;
use std::sync::Arc;

const BS: usize = 4096;
/// `s_sequence`, big-endian, in the JBD2 superblock.
const SEQUENCE_AT: u64 = 0x18;

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

/// Deterministic bytes that differ block to block.
fn pattern(seed: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|j| ((j / BS + seed) as u8).wrapping_mul(31) ^ (j % 251) as u8)
        .collect()
}

fn journal_sequence(fs: &Filesystem) -> u32 {
    let jinode = Inode::parse(&fs.read_inode_raw(fs.sb.journal_inode).unwrap()).unwrap();
    let jsb_block = jbd2::journal_block_to_physical(fs, &jinode, 0)
        .unwrap()
        .unwrap();
    let mut seq = [0u8; 4];
    fs.dev
        .read_at(jsb_block * BS as u64 + SEQUENCE_AT, &mut seq)
        .unwrap();
    u32::from_be_bytes(seq)
}

fn dump(image: &str, path: &str) -> Vec<u8> {
    let dumped = format!("{image}.dump");
    let _ = std::fs::remove_file(&dumped);
    let (code, log) = run("debugfs", &["-R", &format!("dump {path} {dumped}"), image]);
    assert_eq!(code, Some(0), "{log}");
    let got = std::fs::read(&dumped).unwrap_or_else(|e| panic!("debugfs dump: {e}: {log}"));
    let _ = std::fs::remove_file(&dumped);
    got
}

#[test]
fn large_pwrites_cut_to_the_journal_read_back_through_debugfs() {
    let image =
        fs_ext4_test_support::temp_path!("fs_ext4_pwrite_chunking_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(128 * 1024 * 1024))
        .unwrap();
    let (code, log) = run(
        "mkfs.ext4",
        &["-q", "-F", "-b", "4096", "-J", "size=4", &image],
    );
    assert_eq!(code, Some(0), "{log}");

    let two = pattern(3, 2 * 1024 * 1024);
    let twelve = pattern(11, 12 * 1024 * 1024);
    {
        let dev = FileDevice::open_rw(&image).unwrap();
        let fs = Filesystem::mount(Arc::new(dev) as Arc<dyn BlockDevice>).unwrap();
        let journal_blocks = {
            let jinode = Inode::parse(&fs.read_inode_raw(fs.sb.journal_inode).unwrap()).unwrap();
            jinode.size / BS as u64
        };
        assert_eq!(journal_blocks, 1024, "fixture: a 4 MiB journal");

        fs.apply_create("/two.bin", 0o644).unwrap();
        let before = journal_sequence(&fs);
        fs.apply_pwrite("/two.bin", 0, &two).expect("2 MiB pwrite");
        assert_eq!(
            journal_sequence(&fs).wrapping_sub(before),
            1,
            "a 2 MiB write inside a 4 MiB journal is one transaction"
        );

        fs.apply_create("/twelve.bin", 0o644).unwrap();
        let before = journal_sequence(&fs);
        fs.apply_pwrite("/twelve.bin", 0, &twelve)
            .expect("12 MiB pwrite");
        let transactions = journal_sequence(&fs).wrapping_sub(before);
        assert!(
            (3..=6).contains(&transactions),
            "a 12 MiB write over a 4 MiB journal took {transactions} transactions"
        );
    }

    let (code, log) = run("e2fsck", &["-fn", &image]);
    assert_eq!(code, Some(0), "{log}");
    assert!(
        dump(&image, "/two.bin") == two,
        "debugfs read back different bytes for /two.bin"
    );
    assert!(
        dump(&image, "/twelve.bin") == twelve,
        "debugfs read back different bytes for /twelve.bin"
    );
    let _ = std::fs::remove_file(&image);
}
