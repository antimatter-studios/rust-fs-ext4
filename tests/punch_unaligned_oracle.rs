//! An unaligned punch-hole or zero-range touches only its byte range (#388).
//!
//! The kernel frees the blocks wholly inside a punched range and zeroes the
//! partial blocks at its edges in place. This driver rounded the range out to
//! whole blocks and freed those, so `punch(100, 100)` zeroed bytes 0..4096.
//! The result is a consistent volume holding the wrong data, which `e2fsck`
//! cannot see — so the file is read back by `debugfs`, a reader that is not
//! ours, and compared byte for byte.
//!
//! Volumes come from `mkfs.ext4` (with its journal, so the zeroed edges go
//! through a transaction), and `e2fsck -fn` must accept every result. The
//! e2fsprogs tools run in the harness VM; a test fails when it cannot reach
//! them.

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use std::sync::Arc;

const BS: u64 = 4096;

fn mkfs(tag: &str) -> String {
    let path = fs_ext4_test_support::temp_path!(
        "fs_ext4_punch_unaligned_{tag}_{}.img",
        std::process::id()
    );
    std::fs::File::create(&path)
        .and_then(|f| f.set_len(32 * 1024 * 1024))
        .unwrap();
    let out = fs_ext4_test_support::oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096"])
        .arg(&path)
        .output();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

/// `path` as `debugfs` reads it.
fn dump(image: &str, path: &str) -> Vec<u8> {
    let dumped = format!("{image}.dump");
    let _ = std::fs::remove_file(&dumped);
    fs_ext4_test_support::oracle("debugfs")
        .args(["-R", &format!("dump {path} {dumped}"), image])
        .judged()
        .clean("debugfs dump");
    let got = fs_ext4_test_support::read_small_file(&dumped)
        .unwrap_or_else(|e| panic!("debugfs dump: {e}"));
    let _ = std::fs::remove_file(&dumped);
    got
}

#[derive(Clone, Copy)]
enum Op {
    Punch,
    ZeroRange,
}

/// `(tag, op, file blocks, offset, len)`: inside one block, across a whole
/// block with partial edges, an aligned head with a partial tail, and the
/// same span through zero-range.
const CASES: [(&str, Op, u64, u64, u64); 5] = [
    ("inside_one_block", Op::Punch, 2, 100, 100),
    ("across_a_block", Op::Punch, 3, 100, 2 * BS),
    ("aligned_head", Op::Punch, 3, 0, BS + 100),
    ("aligned_tail", Op::Punch, 3, 100, 2 * BS - 100),
    ("zero_range_across", Op::ZeroRange, 3, 100, 2 * BS),
];

#[test]
fn an_unaligned_punch_or_zero_range_changes_only_its_bytes() {
    for (tag, op, blocks, offset, len) in CASES {
        let path = mkfs(tag);
        let size = (blocks * BS) as usize;
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8 + 1).collect();
        {
            let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&path).unwrap())).unwrap();
            let ino = fs.apply_create("/f", 0o644).expect("create");
            fs.apply_replace_file_content("/f", &data).expect("write");
            match op {
                Op::Punch => fs.apply_fallocate_punch_hole(ino, offset, len),
                Op::ZeroRange => fs.apply_fallocate_zero_range(ino, offset, len),
            }
            .unwrap_or_else(|e| panic!("[{tag}] {e:?}"));
        }
        fs_ext4_test_support::assert_e2fsck_clean(&path, &format!("{tag}: e2fsck -fn"));

        let mut want = data.clone();
        want[offset as usize..(offset + len) as usize].fill(0);
        let got = dump(&path, "/f");
        assert_eq!(got.len(), want.len(), "[{tag}] size");
        let wrong: Vec<usize> = (0..want.len()).filter(|&i| got[i] != want[i]).collect();
        assert!(
            wrong.is_empty(),
            "[{tag}] debugfs reads {} bytes other than written-then-zeroed, first at {:?}",
            wrong.len(),
            wrong.first()
        );
        let _ = std::fs::remove_file(&path);
    }
}
