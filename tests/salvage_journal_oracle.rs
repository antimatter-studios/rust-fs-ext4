//! The journal scan against `debugfs logdump`, and a directory brought
//! back from the journal's copy after its block in place is destroyed.
//!
//! The kernel fills `mkfs.ext3` and `mkfs.ext4` volumes in several synced
//! steps, so the journal holds several transactions, and unmounts: the
//! journal is clean, nothing is left to replay, and every copy it holds is
//! of a block already checkpointed. Then:
//!
//! - every copy [`scan_journal`] finds of the kernel's checkpointed
//!   transactions belongs to a committed transaction, and on
//!   `metadata_csum` ext4, whose journal checksums every tag, passes its
//!   tag checksum, which covers the copy's bytes and so where they are;
//!   ext3's journal keeps none;
//! - a transaction `debugfs` writes and leaves for replay, logging a block
//!   that begins with the journal magic, is listed by `debugfs logdump -a`
//!   and found by the scan at the same journal blocks, and each copy
//!   restores to the bytes written, the escaped magic put back.
//!   (`logdump -O`, which lists checkpointed transactions too, reports a
//!   short read past the end of the ring, so it is not used.)
//! - a directory's block zeroed in place (e2fsck reports it) reads back
//!   from the journal's newest copy of that block with every file in it.
//!
//! The tools and the kernel run in the harness VM; the test fails without
//! it.

use fs_core::{BlockRead, FileDevice};
use fs_ext4::salvage::{
    find_superblock, group_descriptors, read_directory, read_inode, scan_dir_block, scan_journal,
    DirContext, Integrity,
};
use fs_ext4_test_support::{guest_kernel_write, oracle};
use std::collections::BTreeSet;

const FILL: &str = r#"
cd "$MNT"
mkdir d
for i in 1 2 3 4 5 6; do echo "file $i" > "d/f$i"; sync; done
mkdir e
touch e/x
sync
"#;

fn make(tag: &str, kind: &str, args: &[&str]) -> String {
    let image = fs_ext4_test_support::temp_path!(
        "fs_ext4_salvage_journal_{tag}_{}.img",
        std::process::id()
    );
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(64 << 20))
        .unwrap();
    let out = oracle(&format!("mkfs.{kind}"))
        .args(["-q", "-F"])
        .args(args)
        .arg(&image)
        .output();
    assert!(
        out.status.success(),
        "[{tag}] mkfs.{kind}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    guest_kernel_write(&image, FILL);
    image
}

/// `FS block X logged at journal block Y` from `debugfs logdump -a`.
fn logged(image: &str) -> BTreeSet<(u64, u64)> {
    let out = oracle("debugfs")
        .args(["-R", "logdump -a"])
        .arg(image)
        .output();
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("FS block ")?;
            let (fs, rest) = rest.split_once(" logged at journal block ")?;
            let journal = rest.split_whitespace().next()?;
            Some((fs.parse().ok()?, journal.parse().ok()?))
        })
        .collect()
}

/// Two blocks of payload, the first beginning with the journal's magic
/// number, so the journal must escape it.
fn payload(block_size: usize) -> Vec<u8> {
    let mut bytes: Vec<u8> = (0..2 * block_size)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).rotate_left(5) as u8)
        .collect();
    bytes[..4].copy_from_slice(&0xC03B_3998u32.to_be_bytes());
    bytes
}

/// A transaction `debugfs` writes and leaves for replay, logging two free
/// blocks near the end of the volume: `logdump -a` lists it, and the scan
/// finds the same copies, the first escaped, both restoring to the payload.
fn written_transaction(tag: &str, image: &str, block_size: u64, blocks: u64) {
    let copy = format!("{image}.jw");
    std::fs::copy(image, &copy).unwrap();
    let data = format!("{image}.payload");
    let payload = payload(block_size as usize);
    std::fs::write(&data, &payload).unwrap();
    let targets = [blocks - 10, blocks - 9];
    let script = format!("jo\njw -b {},{} {data}\njc\n", targets[0], targets[1]);
    let out = oracle("debugfs")
        .args(["-w", "-f", "-"])
        .arg(&copy)
        .stdin(script)
        .output();
    assert!(out.status.success(), "[{tag}] debugfs journal write");
    let theirs = logged(&copy);
    for target in targets {
        assert!(
            theirs.iter().any(|&(fs, _)| fs == target),
            "[{tag}] logdump does not list block {target}: {theirs:?}"
        );
    }
    let source = FileDevice::open(&copy).unwrap();
    let sb = find_superblock(&source).unwrap().superblock.unwrap();
    let scan = scan_journal(&source, &sb).unwrap();
    let ours: BTreeSet<(u64, u64)> = scan
        .copies
        .iter()
        .map(|c| (c.fs_block, c.journal_block))
        .collect();
    let missing: Vec<_> = theirs.difference(&ours).collect();
    assert!(
        missing.is_empty(),
        "[{tag}] logdump lists {missing:?}, the scan does not"
    );
    for (i, target) in targets.into_iter().enumerate() {
        let newest = scan.copies_of(target)[0];
        assert_eq!(newest.escaped, i == 0, "[{tag}] {newest:?}");
        let mut bytes = vec![0u8; block_size as usize];
        (&source as &dyn BlockRead)
            .read_at(newest.location.offset, &mut bytes)
            .unwrap();
        newest.restored(&mut bytes);
        let want = &payload[i * block_size as usize..(i + 1) * block_size as usize];
        assert!(
            bytes == want,
            "[{tag}] block {target} does not restore to the payload"
        );
    }
    let _ = std::fs::remove_file(&copy);
    let _ = std::fs::remove_file(&data);
}

fn check(tag: &str, kind: &str, args: &[&str], checksummed: bool) {
    let image = make(tag, kind, args);
    let source = FileDevice::open(&image).unwrap();
    let sb = find_superblock(&source).unwrap().superblock.unwrap();
    let scan = scan_journal(&source, &sb).unwrap();
    assert!(scan.superblock.is_some(), "[{tag}] {:?}", scan.faults);
    assert!(
        scan.copies.len() > 6,
        "[{tag}] {} copies",
        scan.copies.len()
    );
    let want = if checksummed {
        Integrity::Verified
    } else {
        Integrity::Unchecked
    };
    for copy in &scan.copies {
        assert_eq!(copy.integrity, want, "[{tag}] {copy:?}");
        assert!(copy.committed, "[{tag}] {copy:?}");
    }
    written_transaction(tag, &image, u64::from(sb.block_size()), sb.blocks_count);

    // /d's directory block, zeroed in place, read back from the journal.
    let descriptors = group_descriptors(&source, &sb, 0);
    let root = read_inode(&source, &sb, &descriptors, 2).unwrap();
    let d = read_directory(&source, &sb, &root)
        .entries()
        .find(|e| e.name == b"d")
        .unwrap()
        .inode;
    let d_record = read_inode(&source, &sb, &descriptors, d).unwrap();
    let d_scan = read_directory(&source, &sb, &d_record);
    let (_, location, _) = &d_scan.blocks[0];
    let block = location.offset / u64::from(sb.block_size());
    drop(source);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&image)
        .unwrap();
    std::os::unix::fs::FileExt::write_all_at(
        &file,
        &vec![0u8; location.length as usize],
        location.offset,
    )
    .unwrap();
    drop(file);
    let repaired = format!("{image}.fsck");
    std::fs::copy(&image, &repaired).unwrap();
    oracle("e2fsck")
        .args(["-fy"])
        .arg(&repaired)
        .judged()
        .repaired(&format!("{tag}: zeroed directory"));
    let _ = std::fs::remove_file(&repaired);

    let source = FileDevice::open(&image).unwrap();
    let copies = scan.copies_of(block);
    let newest = copies
        .first()
        .unwrap_or_else(|| panic!("[{tag}] the journal holds no copy of block {block}"));
    let mut bytes = vec![0u8; newest.location.length as usize];
    (&source as &dyn BlockRead)
        .read_at(newest.location.offset, &mut bytes)
        .unwrap();
    newest.restored(&mut bytes);
    let inode = d_record.inode.as_ref().unwrap();
    let names: BTreeSet<Vec<u8>> = scan_dir_block(&bytes, &DirContext::new(&sb, d, inode))
        .entries
        .into_iter()
        .filter(|e| e.live)
        .map(|e| e.name)
        .collect();
    for i in 1..=6 {
        assert!(
            names.contains(format!("f{i}").as_bytes()),
            "[{tag}] the journal's copy of /d lacks f{i}"
        );
    }
    let _ = std::fs::remove_file(&image);
}

#[test]
fn ext3_journal_without_checksums() {
    check("ext3", "ext3", &["-b", "1024"], false);
}

#[test]
fn four_k_ext4_journal_with_checksums() {
    check("ext4", "ext4", &["-b", "4096"], true);
}
