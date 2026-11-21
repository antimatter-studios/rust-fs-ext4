//! The three layers on volumes `mkfs.ext2`, `mkfs.ext3` and `mkfs.ext4`
//! made and the kernel filled, judged by rust-fs-core's conformance checks
//! and against what the kernel lists.
//!
//! - `check_healthy`: the volume layer reports the volume clean, the
//!   primitives and salvage verbs keep their contracts, and the file layer
//!   holds every file with the bytes the kernel was given;
//! - `check_files_against_candidates`: every file the file layer lists is a
//!   salvage candidate whose walk reads back the same bytes;
//! - every file `find` lists in the guest is listed by the file layer with
//!   the kernel's size and SHA-256;
//! - a hard-linked file's two names are both live, named parent claims in
//!   the rebuilt tree (on a volume of its own: the cross-check matches
//!   files to candidates by one name per candidate, rust-fs-core#453);
//! - with one inode overwritten and a directory block zeroed, the volume
//!   opens for salvage, `check_damaged` holds, the damage report names the
//!   zeroed block, and every file outside what was damaged -- the
//!   destroyed directory's own files included, now orphans -- still reads
//!   back with the kernel's hash through the salvage verbs.
//!
//! The kernel runs in the harness VM; the test fails without it.

use fs_core::layers::conformance::{
    check_damaged, check_files_against_candidates, check_healthy, Expected, Limits,
};
use fs_core::layers::{
    CandidateKind, CandidateState, Extent, FileAPI, HealthState, NodeKind, OpenVolume,
    PrimitiveAPI, Severity, VolumeAPI,
};
use fs_core::FileDevice;
use fs_ext4::layers::{Ext4Detail, Ext4Volume};
use fs_ext4_test_support::{guest_kernel_write, oracle, sha256_hex};
use std::collections::BTreeMap;

/// Files whose bytes the test knows: `name` holds `name` repeated.
const KNOWN: [(&str, usize); 5] = [
    ("a.txt", 1),
    ("dir/b.txt", 300),
    ("dir/sub/c.txt", 5000),
    ("dir/sub/d.txt", 70_000),
    ("other/e.txt", 12),
];

fn known_bytes(path: &str, times: usize) -> Vec<u8> {
    path.as_bytes().repeat(times)
}

const FILL: &str = r#"
cd "$MNT"
mkdir -p dir/sub other
for spec in "a.txt 1" "dir/b.txt 300" "dir/sub/c.txt 5000" "dir/sub/d.txt 70000" "other/e.txt 12"; do
  set -- $spec
  # `yes` and `tr` end on SIGPIPE when `head` has its bytes, which
  # pipefail would count as a failure; the contents are checked anyway.
  { yes "$1" | tr -d '\n' | head -c $(( ${#1} * $2 )) > "$1"; } || true
done
head -c 1000000 /dev/urandom > dir/random
ln -s a.txt link
sync
find . -path ./lost+found -prune -o -type f -printf 'file %s %P\n'
find . -path ./lost+found -prune -o -type f -exec sha256sum {} +
"#;

struct Listed {
    sizes: BTreeMap<String, u64>,
    hashes: BTreeMap<String, String>,
}

fn make(tag: &str, kind: &str, args: &[&str]) -> (String, Listed) {
    let image = fs_ext4_test_support::temp_path!("fs_ext4_layers_{tag}_{}.img", std::process::id());
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
    let out = guest_kernel_write(&image, FILL);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut listed = Listed {
        sizes: BTreeMap::new(),
        hashes: BTreeMap::new(),
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("file ") {
            let (size, path) = rest.split_once(' ').unwrap();
            listed.sizes.insert(path.to_string(), size.parse().unwrap());
        } else if let Some((hash, path)) = line.split_once("  ") {
            listed
                .hashes
                .insert(path.trim_start_matches("./").to_string(), hash.to_string());
        }
    }
    assert_eq!(listed.sizes.len(), 6, "[{tag}] {text}");
    (image, listed)
}

/// Every file through the file layer: path to (size, SHA-256).
fn files<V: FileAPI>(volume: &V) -> BTreeMap<String, (u64, String)> {
    let mut found = BTreeMap::new();
    let mut pending = vec![(String::new(), volume.root().unwrap())];
    while let Some((path, dir)) = pending.pop() {
        for entry in volume.read_dir(&dir).unwrap() {
            let child = if path.is_empty() {
                entry.name.clone()
            } else {
                format!("{path}/{}", entry.name)
            };
            match entry.kind {
                NodeKind::Directory if entry.name != "lost+found" => {
                    pending.push((child, entry.node))
                }
                NodeKind::File => {
                    let mut bytes = Vec::new();
                    let copied = volume.copy_to(&entry.node, &mut bytes).unwrap();
                    found.insert(child, (copied, sha256_hex(&bytes)));
                }
                _ => {}
            }
        }
    }
    found
}

fn healthy(tag: &str, image: &str, listed: &Listed) {
    let volume = Ext4Volume::open(FileDevice::open(image).unwrap()).unwrap();
    let mut expected: Vec<Expected> = KNOWN
        .iter()
        .map(|(path, times)| Expected::file(&format!("/{path}"), &known_bytes(path, *times)))
        .collect();
    expected.push(Expected::directory("/dir/sub"));
    expected.push(Expected::absent("/nothing-here"));
    let limits = Limits::default();
    check_healthy(&volume, &expected, &limits).assert_passed();
    check_files_against_candidates(&volume, &limits).assert_passed();

    let ours = files(&volume);
    let theirs: BTreeMap<String, (u64, String)> = listed
        .sizes
        .iter()
        .map(|(path, size)| (path.clone(), (*size, listed.hashes[path].clone())))
        .collect();
    assert_eq!(ours, theirs, "[{tag}] the file layer against the kernel");
    let target = volume.lookup("/link").unwrap();
    assert_eq!(volume.read_link(&target).unwrap(), b"a.txt", "[{tag}]");
}

fn damaged(tag: &str, image: &str, listed: &Listed) {
    let copy = format!("{image}.damaged");
    std::fs::copy(image, &copy).unwrap();
    let (sub_block, other_at, block_size) = {
        let volume = Ext4Volume::open(FileDevice::open(&copy).unwrap()).unwrap();
        let block_size = volume.geometry().unwrap().allocation_unit;
        let sub = volume.lookup("/dir/sub").unwrap();
        let fs = volume.filesystem().unwrap();
        let sub_block = fs
            .map_inode_logical(&fs.stat_ino(sub).unwrap(), 0)
            .unwrap()
            .unwrap();
        // /other's inode, overwritten alone: its neighbours in the inode
        // table hold the root and the other files.
        let other = volume.lookup("/other").unwrap();
        let salvage = volume.salvage().unwrap();
        let mut candidates = Vec::new();
        salvage
            .find_candidates(Extent::new(0, volume.size_bytes()), &mut candidates)
            .unwrap();
        let at = candidates
            .iter()
            .find(|c| matches!(c.detail, Ext4Detail::Inode { ino, .. } if ino == other))
            .unwrap()
            .location
            .offset;
        (sub_block, at, block_size)
    };
    let file = std::fs::OpenOptions::new().write(true).open(&copy).unwrap();
    let zero = vec![0u8; block_size as usize];
    std::os::unix::fs::FileExt::write_all_at(&file, &zero, sub_block * block_size).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&file, &[0xA5u8; 128], other_at).unwrap();
    drop(file);

    let volume = Ext4Volume::open_for_salvage(FileDevice::open(&copy).unwrap()).unwrap();
    check_damaged(&volume, &Limits::default()).assert_passed();
    // The quick verdict reads the structures and the root, which are
    // intact; the full damage report names the zeroed directory block.
    assert_ne!(
        volume.health().unwrap().state,
        HealthState::Unknown,
        "[{tag}]"
    );
    let mut findings = Vec::new();
    volume
        .salvage()
        .unwrap()
        .report_damage(&mut findings)
        .unwrap();
    assert!(
        findings
            .iter()
            .any(|d| d.severity == Severity::Error && d.location.offset / block_size == sub_block),
        "[{tag}] the zeroed directory block is not reported: {findings:#?}"
    );

    // Every file the zeroed blocks did not hold reads back, by candidate.
    let salvage = volume.salvage().unwrap();
    let candidates = salvage.all_candidates().unwrap();
    let recovered: Vec<String> = candidates
        .iter()
        .filter(|c| c.kind == CandidateKind::File && c.state == CandidateState::Live)
        .map(|c| {
            // The walk's own plan: every stored extent at its file offset.
            let walk = salvage.walk_extents(c).unwrap();
            let mut bytes = Vec::new();
            salvage.materialize(&walk.plan(), &mut bytes).unwrap();
            sha256_hex(&bytes)
        })
        .collect();
    for (path, hash) in &listed.hashes {
        if !path.starts_with("dir/sub/") && !path.starts_with("other/") {
            assert!(recovered.contains(hash), "[{tag}] {path} was not recovered");
        }
    }
    for path in ["dir/sub/c.txt", "dir/sub/d.txt"] {
        assert!(
            recovered.contains(&listed.hashes[path]),
            "[{tag}] {path}, orphaned by its directory's loss, was not recovered"
        );
    }
    let _ = std::fs::remove_file(&copy);
}

fn check(tag: &str, kind: &str, args: &[&str]) {
    let (image, listed) = make(tag, kind, args);
    healthy(tag, &image, &listed);
    damaged(tag, &image, &listed);
    let _ = std::fs::remove_file(&image);
}

#[test]
fn ext2_through_the_layers() {
    check("ext2", "ext2", &["-b", "1024"]);
}

#[test]
fn ext3_through_the_layers() {
    check("ext3", "ext3", &["-b", "4096"]);
}

#[test]
fn four_k_ext4_through_the_layers() {
    check("ext4", "ext4", &["-b", "4096"]);
}

#[test]
fn hard_links_are_named_parent_claims() {
    let tag = "hard-link";
    let image = fs_ext4_test_support::temp_path!("fs_ext4_layers_{tag}_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(32 << 20))
        .unwrap();
    let out = oracle("mkfs.ext4").args(["-q", "-F"]).arg(&image).output();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    guest_kernel_write(
        &image,
        r#"
mkdir "$MNT/dir" "$MNT/other"
echo linked > "$MNT/dir/b.txt"
ln "$MNT/dir/b.txt" "$MNT/other/hard"
sync
"#,
    );
    let volume = Ext4Volume::open(FileDevice::open(&image).unwrap()).unwrap();
    // A hard link: one inode, two named parent claims, both live.
    let salvage = volume.salvage().unwrap();
    let candidates = salvage.all_candidates().unwrap();
    let tree = salvage.rebuild_tree(&candidates).unwrap();
    let linked = volume.lookup("/dir/b.txt").unwrap();
    let at = candidates
        .iter()
        .find(|c| matches!(c.detail, Ext4Detail::Inode { ino, .. } if ino == linked))
        .unwrap()
        .location
        .offset;
    let mut names: Vec<Vec<u8>> = tree
        .nodes
        .iter()
        .find(|n| n.candidate == at)
        .unwrap()
        .parents
        .iter()
        .filter(|c| c.state == CandidateState::Live)
        .filter_map(|c| c.name.clone())
        .collect();
    names.sort();
    assert_eq!(names, [b"b.txt".to_vec(), b"hard".to_vec()], "[{tag}]");

    let _ = std::fs::remove_file(&image);
}
