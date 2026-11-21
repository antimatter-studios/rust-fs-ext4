//! The salvage verbs, end to end, against what the kernel wrote: every
//! file recovered through find candidates, rebuild the tree, walk extents
//! and materialize has the path and the SHA-256 the kernel reported for it.
//!
//! The kernel fills `mkfs.ext2`, `mkfs.ext3` and `mkfs.ext4` volumes with a
//! small tree -- nested directories, files of several sizes, a sparse file
//! -- removes one file, and prints every file's inode number, path and
//! hash. Then:
//!
//! - every file the kernel listed is a live candidate, its path follows its
//!   parent claims to the root, and its materialized bytes hash to the
//!   kernel's;
//! - the removed file's inode is a deleted candidate: the kernel keeps its
//!   mode, sets `i_dtime` and truncates it to nothing first, so what it
//!   held is not recoverable through its map;
//! - the damage report of the untouched volume holds no error.
//!
//! Then a copy is damaged the way a failing disk damages a tree: one
//! directory's block is zeroed. `e2fsck` reports it, the damage report
//! names it, the files that directory held become orphans -- and every one
//! of them still materializes to the kernel's hash.
//!
//! The tools and the kernel run in the harness VM; the test fails without
//! it.

use fs_core::{BlockRead, FileDevice};
use fs_ext4::salvage::{
    Candidate, CandidateKind, CandidateState, Ext4Salvage, Extent, Parent, Severity, Tree,
};
use fs_ext4_test_support::{guest_kernel_write, oracle, sha256_hex};
use std::collections::BTreeMap;

const FILL: &str = r#"
cd "$MNT"
mkdir -p top/mid/deep other
head -c 100 /dev/urandom > top/small
head -c 70000 /dev/urandom > top/mid/medium
head -c 3000000 /dev/urandom > top/mid/deep/large
head -c 5000 /dev/urandom > other/one
truncate -s 2000000 other/sparse
head -c 4096 /dev/urandom | dd of=other/sparse bs=4096 seek=300 conv=notrunc status=none
head -c 9000 /dev/urandom > doomed
echo "removed $(stat -c %i doomed)"
rm doomed
sync
find . -path ./lost+found -prune -o -type f -printf 'file %i %P\n'
find . -path ./lost+found -prune -o -type f -exec sha256sum {} +
"#;

struct Kernel {
    /// Inode number to path, for every file.
    files: BTreeMap<u32, String>,
    /// Path to SHA-256.
    hashes: BTreeMap<String, String>,
    removed: u32,
}

fn make(tag: &str, kind: &str, mib: u64, args: &[&str]) -> (String, Kernel) {
    let image =
        fs_ext4_test_support::temp_path!("fs_ext4_salvage_verbs_{tag}_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(mib << 20))
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
    let mut kernel = Kernel {
        files: BTreeMap::new(),
        hashes: BTreeMap::new(),
        removed: 0,
    };
    for line in text.lines() {
        if let Some(ino) = line.strip_prefix("removed ") {
            kernel.removed = ino.trim().parse().unwrap();
        } else if let Some(rest) = line.strip_prefix("file ") {
            let (ino, path) = rest.split_once(' ').unwrap();
            kernel.files.insert(ino.parse().unwrap(), path.to_string());
        } else if let Some((hash, path)) = line.split_once("  ") {
            kernel
                .hashes
                .insert(path.trim_start_matches("./").to_string(), hash.to_string());
        }
    }
    assert_eq!(kernel.files.len(), 5, "[{tag}] the kernel listed {text}");
    assert_eq!(kernel.hashes.len(), 5, "[{tag}] the kernel listed {text}");
    assert_ne!(kernel.removed, 0, "[{tag}]");
    (image, kernel)
}

struct Recovery {
    source: FileDevice,
    salvage: Ext4Salvage,
    candidates: Vec<Candidate>,
    tree: Tree,
}

impl Recovery {
    fn open(image: &str) -> Self {
        let source = FileDevice::open(image).unwrap();
        let salvage = Ext4Salvage::new(&source).unwrap();
        let mut candidates = Vec::new();
        salvage
            .find_candidates(
                &source,
                Extent::new(0, source.size_bytes()),
                &mut candidates,
            )
            .unwrap();
        let tree = salvage.rebuild_tree(&source, &candidates).unwrap();
        Self {
            source,
            salvage,
            candidates,
            tree,
        }
    }

    fn candidate(&self, ino: u32) -> &Candidate {
        self.candidates
            .iter()
            .find(|c| c.ino() == Some(ino))
            .unwrap_or_else(|| panic!("inode {ino} is not a candidate"))
    }

    /// The path the live parent claims give, or `None` for an orphan or a
    /// chain that does not reach the root.
    fn path(&self, ino: u32) -> Option<String> {
        let mut names = Vec::new();
        let mut at = self.candidate(ino).location.offset;
        for _ in 0..64 {
            let node = self.tree.node(at)?;
            if node.ino == 2 {
                names.reverse();
                return Some(names.join("/"));
            }
            let claim = node.parents.iter().find(|c| c.live && !c.name.is_empty())?;
            names.push(String::from_utf8_lossy(&claim.name).into_owned());
            match claim.parent {
                Parent::Candidate(parent) => at = parent,
                Parent::Root => return None,
            }
        }
        None
    }

    fn hash(&self, ino: u32) -> String {
        let walk = self
            .salvage
            .walk_extents(&self.source, self.candidate(ino))
            .unwrap();
        let mut out = Vec::new();
        let report = self
            .salvage
            .materialize(&self.source, &walk.plan(), &mut out)
            .unwrap();
        assert_eq!(report.unreadable(), 0);
        sha256_hex(&out)
    }
}

fn pristine(tag: &str, image: &str, kernel: &Kernel) {
    let recovery = Recovery::open(image);
    for (&ino, path) in &kernel.files {
        let candidate = recovery.candidate(ino);
        assert_eq!(candidate.kind, CandidateKind::File, "[{tag}] {path}");
        assert_eq!(candidate.state, CandidateState::Live, "[{tag}] {path}");
        assert_eq!(
            recovery.path(ino).as_deref(),
            Some(path.as_str()),
            "[{tag}]"
        );
        assert_eq!(recovery.hash(ino), kernel.hashes[path], "[{tag}] {path}");
    }
    let removed = recovery.candidate(kernel.removed);
    assert_eq!(
        removed.state,
        CandidateState::Deleted,
        "[{tag}] {removed:?}"
    );
    assert_eq!(
        removed.kind,
        CandidateKind::File,
        "[{tag}] the kernel keeps the mode"
    );
    assert_eq!(
        removed.size,
        Some(0),
        "[{tag}] the kernel truncates what it deletes"
    );
    let mut damage = Vec::new();
    recovery
        .salvage
        .report_damage(&recovery.source, &mut damage)
        .unwrap();
    let errors: Vec<_> = damage
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert!(errors.is_empty(), "[{tag}] {errors:#?}");
}

/// `top/mid`'s directory block zeroed on a copy.
fn directory_destroyed(tag: &str, image: &str, kernel: &Kernel) {
    let copy = format!("{image}.dir");
    std::fs::copy(image, &copy).unwrap();
    let recovery = Recovery::open(&copy);
    let mid = recovery
        .tree
        .nodes
        .iter()
        .find(|n| n.parents.iter().any(|c| c.name == b"mid"))
        .unwrap()
        .ino;
    let walk = recovery
        .salvage
        .walk_extents(&recovery.source, recovery.candidate(mid))
        .unwrap();
    let block = walk.extents[0];
    drop(recovery);
    let file = std::fs::OpenOptions::new().write(true).open(&copy).unwrap();
    std::os::unix::fs::FileExt::write_all_at(
        &file,
        &vec![0u8; block.length as usize],
        block.offset,
    )
    .unwrap();
    drop(file);

    let repaired = format!("{copy}.fsck");
    std::fs::copy(&copy, &repaired).unwrap();
    oracle("e2fsck")
        .args(["-fy"])
        .arg(&repaired)
        .judged()
        .repaired(&format!("{tag}: zeroed directory"));
    let _ = std::fs::remove_file(&repaired);

    let recovery = Recovery::open(&copy);
    let mut damage = Vec::new();
    recovery
        .salvage
        .report_damage(&recovery.source, &mut damage)
        .unwrap();
    assert!(
        damage
            .iter()
            .any(|d| d.structure.starts_with(&format!("directory inode {mid},"))),
        "[{tag}] {damage:#?}"
    );
    let medium = *kernel
        .files
        .iter()
        .find(|(_, path)| path.as_str() == "top/mid/medium")
        .unwrap()
        .0;
    let node = recovery
        .tree
        .node(recovery.candidate(medium).location.offset)
        .unwrap();
    assert!(node.parents.is_empty(), "[{tag}] {node:?}");
    for (&ino, path) in &kernel.files {
        assert_eq!(
            recovery.hash(ino),
            kernel.hashes[path],
            "[{tag}] {path} after the damage"
        );
        if !path.starts_with("top/mid/") {
            assert_eq!(
                recovery.path(ino).as_deref(),
                Some(path.as_str()),
                "[{tag}]"
            );
        }
    }
    let _ = std::fs::remove_file(&copy);
}

fn check(tag: &str, kind: &str, mib: u64, args: &[&str]) {
    let (image, kernel) = make(tag, kind, mib, args);
    pristine(tag, &image, &kernel);
    directory_destroyed(tag, &image, &kernel);
    let _ = std::fs::remove_file(&image);
}

#[test]
fn ext2_volume_recovered() {
    check("ext2", "ext2", 32, &["-b", "1024"]);
}

#[test]
fn ext3_volume_recovered() {
    check("ext3", "ext3", 64, &["-b", "4096"]);
}

#[test]
fn four_k_ext4_volume_recovered() {
    check("ext4", "ext4", 64, &["-b", "4096"]);
}
