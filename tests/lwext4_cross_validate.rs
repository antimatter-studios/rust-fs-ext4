//! CROSS-VALIDATION AGAINST A THIRD IMPLEMENTATION (#99).
//!
//! `verify::verify` checks this crate against its own reading of the
//! specification, so it cannot see a misreading. `e2fsck` and `debugfs`
//! are a second reader written by the project that wrote the kernel's
//! driver, from the same document — an ambiguity they and we resolve the
//! same way is one neither of us can report. The kernel is the authority
//! the images are for, which makes it the answer rather than an
//! independent check of it.
//!
//! lwext4 is none of those. BSD-2-Clause, pure C, no code lineage in
//! common with the kernel or with this crate, and complete enough to
//! mount, walk and write ext2/3/4. Where it and this crate disagree
//! about the same bytes, ONE OF THE TWO HAS READ THE FORMAT WRONG, and
//! finding that out is the point.
//!
//! WHAT IS COMPARED, IN BOTH DIRECTIONS:
//!
//!   * every fixture lwext4's feature set covers — kernel-made bytes,
//!     read by this crate and by lwext4, compared name by name on type,
//!     permission bits, size, SHA-256 of contents and symlink target;
//!   * this crate writes a tree (nested directories, a multi-extent file
//!     written in unaligned pieces, an empty file, a fast symlink, a
//!     long symlink, a directory big enough to be indexed, a rename, an
//!     unlink, a truncate) and lwext4 reads it back;
//!   * lwext4 writes a tree and this crate reads it back.
//!
//! AND THE FIXTURES IT CANNOT READ ARE NAMED, NOT SKIPPED. lwext4
//! implements neither `inline_data`, nor `large_dir`, nor
//! `metadata_csum_seed`, and it is not a partition table reader. Each of
//! those images is asserted to be REFUSED, so an lwext4 that grows the
//! feature — or a fixture that loses it — fails here and gets moved into
//! the compared set, instead of quietly never being compared.
//!
//! AND IT HAS ALREADY FOUND ONE. lwext4 reads a hole in the middle of a
//! file as physical block 0 of the device rather than as zeroes:
//! `ext4_fread` zero-fills only the leading partial block, and hands an
//! `fblock_start` of 0 to `ext4_blocks_get_direct` for a run of unmapped
//! blocks. `ext4-deep-extents.img`'s sparse file comes back with the
//! volume's first blocks in its holes. `KNOWN` records it, with the
//! question of who is wrong settled by the fixture's own recipe rather
//! than by either reader, and REQUIRES IT TO STILL BE WRONG — a pin bump
//! that fixes lwext4 deletes the entry instead of absorbing it.
//!
//! WHERE IT RUNS: the fs-linux-test-harness guest, like every other
//! oracle in this suite. `scripts/vm-setup.sh` builds lwext4 there at a
//! pinned commit and `tests/support/src/lwext4.rs` compiles
//! `tests/lwext4/report.c` against it. Nothing runs on the host, nothing
//! is installed on it, and a guest without lwext4 fails these tests
//! naming `chore vm:provision`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::fs::Filesystem;
use fs_ext4::{dir, features, file_io};
use fs_ext4_test_support::{
    fixture, lwext4_refusal, lwext4_report, lwext4_write, oracle, sha256_hex, temp_path, Report,
    LWEXT4_PIN,
};

// ---------------------------------------------------------------------
// This crate's own view, in the shape lwext4 reports
// ---------------------------------------------------------------------

/// One directory's entries, `.` and `..` and tombstones left out.
fn entries(fs: &Filesystem, ino: u32) -> Vec<(Vec<u8>, u32)> {
    let (inode, _) = fs
        .read_inode_verified(ino)
        .unwrap_or_else(|e| panic!("read directory inode {ino}: {e:?}"));
    let data =
        file_io::read_all(fs, &inode).unwrap_or_else(|e| panic!("read directory {ino}: {e:?}"));
    let has_file_type = fs.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
    let block_size = fs.sb.block_size() as usize;
    data.chunks(block_size)
        .flat_map(|block| {
            // A block that does not parse is a failure, not an empty
            // one: skipping it would shrink the comparison and still
            // report agreement.
            dir::parse_block(block, has_file_type)
                .unwrap_or_else(|e| panic!("a block of directory {ino} does not parse: {e:?}"))
        })
        .filter(|entry| entry.inode != 0 && entry.name != b"." && entry.name != b"..")
        .map(|entry| (entry.name, entry.inode))
        .collect()
}

/// A symlink's target, from wherever this crate stores it.
fn target(fs: &Filesystem, inode: &fs_ext4::inode::Inode) -> String {
    let bytes = if inode.size < 60 {
        inode.block[..inode.size as usize].to_vec()
    } else {
        file_io::read_all(fs, inode).unwrap_or_else(|e| panic!("read symlink target: {e:?}"))
    };
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The type names `tests/lwext4/report.c` prints, from the INODE rather
/// than from the directory entry: a directory entry whose file type
/// disagrees with its inode is a defect of exactly the kind this test
/// exists to find, and taking the same field from both sides would hide
/// it.
fn type_name(inode: &fs_ext4::inode::Inode) -> &'static str {
    if inode.is_dir() {
        "directory"
    } else if inode.is_symlink() {
        "symbolic-link"
    } else if inode.is_file() {
        "regular-file"
    } else {
        "other"
    }
}

fn walk(fs: &Filesystem, ino: u32, prefix: &str, out: &mut Report) {
    for (name, child) in entries(fs, ino) {
        let path = format!("{prefix}{}", String::from_utf8_lossy(&name));
        let (inode, _) = fs
            .read_inode_verified(child)
            .unwrap_or_else(|e| panic!("read inode {child} ({path}): {e:?}"));
        out.insert(("type".into(), path.clone()), type_name(&inode).into());
        out.insert(
            ("mode".into(), path.clone()),
            format!("{:o}", inode.mode & 0o7777),
        );
        if inode.is_file() {
            let data =
                file_io::read_all(fs, &inode).unwrap_or_else(|e| panic!("read {path}: {e:?}"));
            out.insert(("size".into(), path.clone()), inode.size.to_string());
            out.insert(("sha256".into(), path.clone()), sha256_hex(&data));
        } else if inode.is_symlink() {
            out.insert(("target".into(), path.clone()), target(fs, &inode));
        } else if inode.is_dir() {
            walk(fs, child, &format!("{path}/"), out);
        }
    }
}

/// EVERYTHING THIS CRATE SEES IN `image`, in the shape lwext4 reports it.
fn ours(image: &str) -> Report {
    let device =
        FileDevice::open(image).unwrap_or_else(|e| panic!("open {image} read-only: {e:?}"));
    let fs = Filesystem::mount(Arc::new(device) as Arc<dyn BlockDevice>)
        .unwrap_or_else(|e| panic!("mount {image}: {e:?}"));
    let mut out = BTreeMap::new();
    walk(&fs, fs_ext4::path::EXT4_ROOT_INODE, "", &mut out);
    out
}

/// Every place the two views differ, named.
fn disagreements(ours: &Report, theirs: &Report) -> Vec<String> {
    let keys: BTreeSet<&(String, String)> = ours.keys().chain(theirs.keys()).collect();
    keys.into_iter()
        .filter_map(|key| {
            let (kind, path) = key;
            match (ours.get(key), theirs.get(key)) {
                (Some(a), Some(b)) if a == b => None,
                (Some(a), Some(b)) => {
                    Some(format!("{kind} of {path}: fs-ext4 {a:?}, lwext4 {b:?}"))
                }
                (Some(a), None) => Some(format!("{kind} of {path}: fs-ext4 {a:?}, lwext4 nothing")),
                (None, Some(b)) => Some(format!("{kind} of {path}: fs-ext4 nothing, lwext4 {b:?}")),
                (None, None) => unreachable!("the key came from one of the two maps"),
            }
        })
        .collect()
}

/// DIFFERENCES ALREADY RUN DOWN, AND WHICH OF THE TWO IS WRONG.
///
/// A cross-validator with one known difference in it is a cross-validator
/// that gets switched off, so each one is recorded — with the answer
/// settled by something that is NEITHER implementation — rather than
/// tolerated in silence.
///
/// EVERY ENTRY IS REQUIRED TO STILL DISAGREE. One whose two sides have
/// come back into line fails the run, so bumping the lwext4 pin to a
/// revision that fixes the defect deletes the entry instead of quietly
/// absorbing it. `(image, kind, path, what is wrong and who is wrong)`.
const KNOWN: [(&str, &str, &str, &str); 1] = [(
    "ext4-deep-extents.img",
    "sha256",
    "sparse.bin",
    "lwext4 reads A HOLE IN THE MIDDLE OF A FILE as physical block 0 of the \
     device. `ext4_fread`'s whole-block loop (src/ext4.c) zero-fills only the \
     leading partial block; for a run of unmapped blocks it leaves \
     `fblock_start` at 0 and hands that to `ext4_blocks_get_direct`, which \
     reads the volume's first blocks and returns them as the file's contents. \
     THIS CRATE IS THE ONE THAT IS RIGHT: \
     `the_sparse_fixture_reads_as_it_was_built` reconstructs the 16 MiB the \
     fixture recipe describes, from the recipe and not from either reader, \
     and this crate's digest is that one.",
)];

/// Every disagreement about `image`, with the recorded ones taken out —
/// and a complaint for any recorded one that has stopped disagreeing.
fn compare(image: &str, ours: &Report, theirs: &Report) -> Vec<String> {
    let mut wrong = Vec::new();
    for difference in disagreements(ours, theirs) {
        wrong.push(format!("[{image}] {difference}"));
    }
    // A recorded difference is removed from the list by the exact key it
    // names, so a DIFFERENT difference about the same file is still
    // reported.
    wrong.retain(|line| {
        !KNOWN
            .iter()
            .any(|(i, k, p, _)| *i == image && line.starts_with(&format!("[{i}] {k} of {p}: ")))
    });
    for (i, kind, path, _) in KNOWN {
        if i != image {
            continue;
        }
        let key = (kind.to_string(), path.to_string());
        if ours.get(&key) == theirs.get(&key) {
            wrong.push(format!(
                "[{image}] {kind} of {path} is recorded in KNOWN as a place this crate and \
                 lwext4 disagree, and they now agree. Delete the entry -- a known \
                 difference that has gone away must not keep excusing the next one."
            ));
        }
        if !ours.contains_key(&key) {
            wrong.push(format!(
                "[{image}] {kind} of {path} is recorded in KNOWN and this crate does not \
                 report it at all; the entry no longer describes anything."
            ));
        }
    }
    if theirs.is_empty() {
        wrong.push(format!("[{image}] lwext4 reported nothing at all"));
    }
    wrong
}

// ---------------------------------------------------------------------
// The fixtures
// ---------------------------------------------------------------------

/// The kernel-made images lwext4's feature set covers. EVERY FIXTURE IS
/// IN THIS LIST OR IN `REFUSED`, and `no_fixture_is_left_uncompared`
/// fails if one is in neither — a fixture added later is compared or
/// explained, never simply absent.
const COMPARED: [&str; 6] = [
    "ext4-basic.img",
    "ext4-htree.img",
    "ext4-no-csum.img",
    "ext4-deep-extents.img",
    "ext4-acl.img",
    "ext4-manyfiles.img",
];

/// The images lwext4 must refuse, and why each one.
const REFUSED: [(&str, &str); 5] = [
    ("ext4-inline.img", "inline_data"),
    ("ext4-xattr.img", "inline_data"),
    ("ext4-largedir.img", "large_dir"),
    ("ext4-csum-seed.img", "metadata_csum_seed"),
    // A GPT, not a volume: lwext4 reads no partition table, so what it
    // finds at offset zero is not a superblock.
    ("ext4-whole-disk.img", "a partition table"),
];

fn image(name: &str) -> String {
    fixture(env!("CARGO_MANIFEST_DIR"), name)
}

#[test]
fn every_fixture_lwext4_can_read_reads_the_same_through_both_implementations() {
    // EVERY IMAGE, THEN ONE VERDICT. Failing at the first disagreement
    // would hide the rest behind it, and the interesting question when
    // two implementations differ is how widely.
    let mut wrong = Vec::new();
    for name in COMPARED {
        let path = image(name);
        let theirs = lwext4_report(&path, name);
        wrong.extend(compare(name, &ours(&path), &theirs));
    }
    assert!(
        wrong.is_empty(),
        "this crate and lwext4 disagree about {} thing(s) not recorded in KNOWN:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// WHO IS RIGHT ABOUT THE HOLES, settled by neither reader.
///
/// `test-disks/guest-build-images.sh` builds `sparse.bin` as a 16 MiB
/// file with a single `X` written every 64 KiB below 16,000,000 and
/// nothing else — so its contents are known from the recipe, and the
/// digest below is computed from that description rather than read from
/// any implementation. This crate's answer is that one; lwext4's is not
/// (see `KNOWN`), which is what makes that entry a defect of lwext4's
/// and not a difference of opinion.
#[test]
fn the_sparse_fixture_reads_as_it_was_built() {
    let path = image("ext4-deep-extents.img");
    let mut expected = vec![0u8; 16 * 1024 * 1024];
    let mut at = 0usize;
    while at < 16_000_000 {
        expected[at] = b'X';
        at += 65536;
    }
    let ours = ours(&path);
    assert_eq!(
        ours.get(&("sha256".to_string(), "sparse.bin".to_string())),
        Some(&sha256_hex(&expected)),
        "this crate does not read sparse.bin as test-disks/guest-build-images.sh \
         wrote it, so it is in no position to call lwext4 wrong about it"
    );
    assert_eq!(
        ours.get(&("size".to_string(), "sparse.bin".to_string())),
        Some(&expected.len().to_string())
    );
}

#[test]
fn the_fixtures_lwext4_does_not_implement_are_refused_by_name() {
    for (name, feature) in REFUSED {
        let path = image(name);
        let said = lwext4_refusal(&path, name);
        assert!(
            said.contains("ext4_mount"),
            "[{name}] lwext4 was expected to refuse the volume at mount time \
             (it does not implement {feature}); it said: {said}"
        );
    }
}

/// A PATH THAT NAMES NOTHING IS NOT A REFUSAL (#280).
///
/// lwext4 opens its file lazily, so a missing image comes back as
/// `ext4_mount (read-only): 5 (Input/output error)` — word for word what
/// a refusal of a real volume prints, and exactly what the test above
/// accepts. `lwext4_refusal` used to return it as one.
#[test]
#[should_panic(expected = "does not exist")]
fn a_missing_image_is_not_a_refusal() {
    let path = temp_path!("fs_ext4_lwext4_missing_{}.img", std::process::id());
    let _ = std::fs::remove_file(&path);
    let said = lwext4_refusal(&path, "an image that does not exist");
    panic!("a missing image was taken for a refusal: {said}");
}

/// NOTHING FALLS BETWEEN THE TWO LISTS. A fixture added to
/// `test-disks/build-fixtures.sh` and to neither list above would be one
/// lwext4 never sees, and nothing else in the suite would say so.
#[test]
fn no_fixture_is_left_uncompared() {
    let known: BTreeSet<&str> = COMPARED
        .into_iter()
        .chain(REFUSED.into_iter().map(|(name, _)| name))
        .collect();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks");
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read the fixture directory") {
        let name = entry.expect("fixture entry").file_name();
        let name = name.to_string_lossy().into_owned();
        if name.ends_with(".img") && !known.contains(name.as_str()) {
            missing.push(name);
        }
    }
    missing.sort();
    assert!(
        missing.is_empty(),
        "these fixtures are neither cross-validated against lwext4 nor listed as \
         refused by it: {}. Add each to COMPARED, or to REFUSED with the feature \
         lwext4 lacks.",
        missing.join(", ")
    );
}

// ---------------------------------------------------------------------
// This crate writes, lwext4 reads
// ---------------------------------------------------------------------

/// A volume lwext4 can mount, made by `mke2fs` in the guest.
///
/// `^metadata_csum_seed` AND `^orphan_file` are not decoration:
/// e2fsprogs turns both on by default, and lwext4 implements neither, so
/// a default `mkfs.ext4` volume is one it refuses outright.
/// `^has_journal` keeps the two directions honest about the same thing —
/// lwext4 does not replay a journal, so a comparison over one would be a
/// comparison of who replayed rather than of who reads.
fn volume(tag: &str) -> String {
    let image = temp_path!("fs_ext4_lwext4_{tag}_{}.img", std::process::id());
    let _ = std::fs::remove_file(&image);
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .unwrap_or_else(|e| panic!("size {image}: {e}"));
    let out = oracle("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-b",
            "4096",
            "-O",
            "^has_journal,^metadata_csum_seed,^orphan_file",
            &image,
        ])
        .output();
    assert!(
        out.status.success(),
        "mkfs.ext4 {image}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    image
}

/// Bytes whose every 32-bit window differs, so a block read from the
/// wrong place is a different hash rather than plausible data.
fn payload(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    let mut state = 0x1234_5678_9abc_def0u64;
    while out.len() < len {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

const LONG_TARGET: &str =
    "a/very/long/target/that/cannot/live/inside/the/inode/because/it/is/far/past/sixty/bytes";

/// The tree this crate writes, and what lwext4 must therefore see.
///
/// THE EXPECTATION IS WHAT WAS WRITTEN, not what this crate reads back:
/// a writer and a reader that share a misreading agree with each other,
/// and the whole point of a third implementation is to not be asked.
fn write_tree(image: &str) -> Report {
    let big = payload(5 * 1024 * 1024 + 777);
    let small = b"a small file, written whole\n".to_vec();
    let renamed = b"renamed content\n".to_vec();
    let truncated = payload(200_000)[..4097].to_vec();

    let fs = Filesystem::mount(Arc::new(
        FileDevice::open_rw(image).unwrap_or_else(|e| panic!("open {image} rw: {e:?}")),
    ) as Arc<dyn BlockDevice>)
    .expect("mount to write");

    fs.apply_mkdir("/dir", 0o755).expect("mkdir /dir");
    fs.apply_mkdir("/dir/nested", 0o700).expect("mkdir nested");

    fs.apply_create("/dir/small.txt", 0o644).expect("create");
    fs.apply_replace_file_content("/dir/small.txt", &small)
        .expect("write small");
    fs.apply_create("/dir/empty.bin", 0o640).expect("create");

    // The big file in pieces that straddle block and extent boundaries
    // rather than filling them.
    fs.apply_create("/dir/big.bin", 0o600).expect("create big");
    let mut at = 0usize;
    for len in [3usize, 4093, 1_048_577, 7, 4096 * 300 + 11] {
        let end = (at + len).min(big.len());
        fs.apply_pwrite("/dir/big.bin", at as u64, &big[at..end])
            .expect("pwrite chunk");
        at = end;
    }
    if at < big.len() {
        fs.apply_pwrite("/dir/big.bin", at as u64, &big[at..])
            .expect("pwrite tail");
    }

    fs.apply_symlink("dir/big.bin", "/link-to-big")
        .expect("symlink");
    fs.apply_symlink(LONG_TARGET, "/long-link")
        .expect("long symlink");

    // A rename, an unlink and a truncate: states a consistency check
    // cannot see but a reader trips over.
    fs.apply_create("/dir/to-rename", 0o644).expect("create");
    fs.apply_replace_file_content("/dir/to-rename", &renamed)
        .expect("write");
    fs.apply_rename("/dir/to-rename", "/dir/nested/renamed", false)
        .expect("rename");

    fs.apply_create("/dir/to-unlink", 0o644).expect("create");
    fs.apply_replace_file_content("/dir/to-unlink", b"gone")
        .expect("write");
    fs.apply_unlink("/dir/to-unlink").expect("unlink");

    fs.apply_create("/dir/truncated", 0o644).expect("create");
    fs.apply_replace_file_content("/dir/truncated", &payload(200_000))
        .expect("write");
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    let ino = fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, "/dir/truncated")
        .expect("resolve truncated");
    fs.apply_truncate_shrink(ino, 4097).expect("truncate");

    // A directory past one block, so lwext4 walks a second block and —
    // once this crate indexes it — an htree rather than a linear list.
    fs.apply_mkdir("/many", 0o755).expect("mkdir many");
    let mut expected = Report::new();
    for i in 0..200u32 {
        let name = format!("/many/entry-{i:04}");
        let body = payload(i as usize % 97);
        fs.apply_create(&name, 0o644).expect("create many");
        if !body.is_empty() {
            fs.apply_replace_file_content(&name, &body)
                .expect("write many");
        }
        let path = name.trim_start_matches('/').to_string();
        expected.insert(("type".into(), path.clone()), "regular-file".into());
        expected.insert(("mode".into(), path.clone()), "644".into());
        expected.insert(("size".into(), path.clone()), body.len().to_string());
        expected.insert(("sha256".into(), path.clone()), sha256_hex(&body));
    }
    drop(fs);

    let mut want = |kind: &str, path: &str, value: String| {
        expected.insert((kind.to_string(), path.to_string()), value);
    };
    want("type", "dir", "directory".into());
    want("mode", "dir", "755".into());
    want("type", "dir/nested", "directory".into());
    want("mode", "dir/nested", "700".into());
    want("type", "many", "directory".into());
    want("mode", "many", "755".into());

    want("type", "dir/small.txt", "regular-file".into());
    want("mode", "dir/small.txt", "644".into());
    want("size", "dir/small.txt", small.len().to_string());
    want("sha256", "dir/small.txt", sha256_hex(&small));

    want("type", "dir/empty.bin", "regular-file".into());
    want("mode", "dir/empty.bin", "640".into());
    want("size", "dir/empty.bin", "0".into());
    want("sha256", "dir/empty.bin", sha256_hex(b""));

    want("type", "dir/big.bin", "regular-file".into());
    want("mode", "dir/big.bin", "600".into());
    want("size", "dir/big.bin", big.len().to_string());
    want("sha256", "dir/big.bin", sha256_hex(&big));

    want("type", "link-to-big", "symbolic-link".into());
    want("target", "link-to-big", "dir/big.bin".into());
    want("type", "long-link", "symbolic-link".into());
    want("target", "long-link", LONG_TARGET.into());

    want("type", "dir/nested/renamed", "regular-file".into());
    want("size", "dir/nested/renamed", renamed.len().to_string());
    want("sha256", "dir/nested/renamed", sha256_hex(&renamed));

    want("type", "dir/truncated", "regular-file".into());
    want("size", "dir/truncated", "4097".into());
    want("sha256", "dir/truncated", sha256_hex(&truncated));

    expected
}

#[test]
fn lwext4_reads_back_what_this_crate_wrote() {
    let image = volume("write");
    let expected = write_tree(&image);
    let theirs = lwext4_report(&image, "fs-ext4 wrote it");

    let mut wrong = Vec::new();
    for (key, value) in &expected {
        let (kind, path) = key;
        match theirs.get(key) {
            Some(got) if got == value => {}
            Some(got) => wrong.push(format!(
                "{kind} of {path}: fs-ext4 wrote {value:?}, lwext4 read {got:?}"
            )),
            None => wrong.push(format!("{kind} of {path}: lwext4 did not see it at all")),
        }
    }
    for gone in ["dir/to-unlink", "dir/to-rename"] {
        if theirs.contains_key(&("type".to_string(), gone.to_string())) {
            wrong.push(format!("{gone} was removed, and lwext4 still sees it"));
        }
    }
    assert!(
        wrong.is_empty(),
        "lwext4 read back {} thing(s) differently from what this crate wrote:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
    let _ = std::fs::remove_file(&image);
}

// ---------------------------------------------------------------------
// lwext4 writes, this crate reads
// ---------------------------------------------------------------------

#[test]
fn this_crate_reads_back_what_lwext4_wrote() {
    let image = volume("read");
    // lwext4's own account of the tree it just created, which is the
    // expectation: it is the writer here, so nothing of ours is being
    // compared against anything else of ours.
    let theirs = lwext4_write(&image, "lwext4 wrote it");
    assert!(
        theirs.len() > 100,
        "lwext4 reported writing only {} things; the tree it writes is larger than that",
        theirs.len()
    );
    let ours = ours(&image);

    let mut wrong = Vec::new();
    for (key, value) in &theirs {
        let (kind, path) = key;
        match ours.get(key) {
            Some(got) if got == value => {}
            Some(got) => wrong.push(format!(
                "{kind} of {path}: lwext4 wrote {value:?}, fs-ext4 read {got:?}"
            )),
            None => wrong.push(format!("{kind} of {path}: fs-ext4 did not see it at all")),
        }
    }
    assert!(
        wrong.is_empty(),
        "this crate read back {} thing(s) differently from what lwext4 wrote:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
    let _ = std::fs::remove_file(&image);
}

// ---------------------------------------------------------------------
// The comparison can fail
// ---------------------------------------------------------------------

/// A COMPARISON THAT CANNOT FAIL IS NOT A COMPARISON (#99).
///
/// The old version of this file reported success having compared
/// nothing, which is exactly what a green cross-validation run looks
/// like from the outside. So one byte of one file's data is flipped —
/// under `metadata_csum`, which covers metadata and not contents, so
/// nothing on either side is entitled to notice it by checksum — and
/// lwext4 must come back with a different digest from the one this crate
/// wrote. If it does not, the two reports are not being compared at all.
#[test]
fn one_flipped_data_byte_makes_the_two_disagree() {
    let image = volume("corrupt");
    let body = payload(8192);

    let block = {
        let fs = Filesystem::mount(Arc::new(
            FileDevice::open_rw(&image).unwrap_or_else(|e| panic!("open {image} rw: {e:?}")),
        ) as Arc<dyn BlockDevice>)
        .expect("mount to write");
        fs.apply_create("/file.bin", 0o644).expect("create");
        fs.apply_replace_file_content("/file.bin", &body)
            .expect("write");
        let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
        let ino = fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, "/file.bin")
            .expect("resolve /file.bin");
        let (inode, _) = fs.read_inode_verified(ino).expect("read inode");
        let physical = fs
            .map_inode_logical(&inode, 0)
            .expect("map block 0")
            .expect("block 0 of a written file is allocated");
        physical * fs.sb.block_size() as u64
    };

    // Uncompared first: the digest must agree BEFORE the flip, or the
    // disagreement afterwards would prove nothing about the flip.
    let clean = lwext4_report(&image, "before the flip");
    assert_eq!(
        clean.get(&("sha256".to_string(), "file.bin".to_string())),
        Some(&sha256_hex(&body)),
        "lwext4 and this crate disagreed before anything was corrupted"
    );

    let mut bytes = std::fs::read(&image).expect("read the image");
    bytes[block as usize + 17] ^= 0xff;
    std::fs::write(&image, &bytes).expect("write the image back");

    let dirty = lwext4_report(&image, "after the flip");
    assert_eq!(
        dirty.get(&("size".to_string(), "file.bin".to_string())),
        Some(&body.len().to_string()),
        "the flip changed the size, so it did not land in the file's data"
    );
    assert_ne!(
        dirty.get(&("sha256".to_string(), "file.bin".to_string())),
        Some(&sha256_hex(&body)),
        "one byte of file.bin's data was flipped and lwext4 still reports the \
         digest of what this crate wrote -- the comparison is not comparing"
    );
    let _ = std::fs::remove_file(&image);
}

// ---------------------------------------------------------------------
// The pin
// ---------------------------------------------------------------------

/// THE REVISION THE GUEST BUILDS IS THE REVISION THIS SUITE TALKS TO.
///
/// `scripts/vm-setup.sh` builds lwext4 and stamps the commit it built;
/// `tests/support/src/lwext4.rs` refuses a guest stamped with anything
/// else. Both strings are written by hand, so this is what keeps them
/// one string: a bump that touches only one of them fails here rather
/// than leaving every lwext4 test failing to start in the harness.
#[test]
fn the_pin_the_guest_builds_is_the_pin_this_suite_requires() {
    let setup =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/vm-setup.sh"))
            .expect("read scripts/vm-setup.sh");
    assert!(
        setup.contains(&format!("LWEXT4_PIN={LWEXT4_PIN}")),
        "scripts/vm-setup.sh does not build lwext4 at {LWEXT4_PIN}, which is the \
         revision tests/support/src/lwext4.rs requires of the guest"
    );
    assert_eq!(
        LWEXT4_PIN.len(),
        40,
        "the lwext4 pin must be a full commit SHA, not a prefix or a branch: {LWEXT4_PIN}"
    );
}
