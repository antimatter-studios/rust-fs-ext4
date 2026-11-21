//! The salvage block maps and directory scans against `debugfs`, `e2fsck`
//! and the kernel, on volumes `mkfs.ext2`, `mkfs.ext3` and `mkfs.ext4` made
//! and the kernel filled.
//!
//! The kernel writes, into each volume: a directory of forty files, two of
//! them then removed; a file written a block at a time between writes to
//! another, so its map is fragmented (an extent tree below the inode on
//! ext4, indirect blocks on ext2 and ext3); a 3 MiB file, which reaches the
//! double-indirect block on 1 KiB-block ext2; and a short and a long
//! symlink. It prints each file's SHA-256 as it goes.
//!
//! For each file, found through [`read_directory`] from the root:
//! - the blocks its map names, data and map blocks together, are the ones
//!   `debugfs blocks` lists, and the map blocks are the extent-tree and
//!   indirect blocks `debugfs stat` names;
//! - the bytes the map names, cut to `i_size`, hash to what the kernel
//!   printed;
//! - a directory's live entries are the ones `debugfs ls` lists, and its
//!   deleted ones are the named ones `debugfs ls -d` adds. The kernel in
//!   the harness VM zeroes the bytes of an entry it removes from a linear
//!   directory, so `rm` leaves none; splitting a hashed directory's leaf
//!   leaves the moved entries' old copies in the slack, and both find
//!   those ([`inline_and_hashed_directories`]).
//!
//! Then copies of each volume are damaged -- the fragmented file's first
//! map block zeroed, and a record length in the directory broken --
//! `e2fsck` confirms each, and the salvage reads are judged against the
//! pristine ones: the map stops at the zeroed block and says why, and the
//! directory loses no entry but the one whose record broke.
//!
//! The tools and the kernel run in the harness VM; the test fails without
//! it.

use fs_core::{BlockRead, FileDevice};
use fs_ext4::salvage::{
    find_superblock, group_descriptors, map_inode, read_directory, read_inode, BlockMap,
    DamageKind, DirectoryScan, InodeRecord, MapEnd,
};
use fs_ext4::Superblock;
use fs_ext4_test_support::{guest_kernel_write, oracle};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::FileExt;

/// The kernel's writes, and the SHA-256 of each regular file.
const FILL: &str = r#"
bs=$(stat -f -c %S "$MNT")
mkdir "$MNT/d"
for i in $(seq 0 39); do echo "file $i" > "$MNT/d/f$i"; done
rm "$MNT/d/f7" "$MNT/d/f20"
for i in $(seq 0 29); do
  dd if=/dev/urandom of="$MNT/frag" bs="$bs" count=1 oflag=append,sync conv=notrunc status=none
  dd if=/dev/urandom of="$MNT/pad$i" bs="$bs" count=1 oflag=sync status=none
done
dd if=/dev/urandom of="$MNT/big" bs=1024 count=3072 status=none
ln -s short "$MNT/s1"
ln -s "$(printf 'long/%.0s' $(seq 1 30))" "$MNT/s2"
sync
cd "$MNT"
sha256sum frag big d/f0 d/f39
"#;

/// A volume `mkfs.<kind>` makes and the kernel fills; the hashes it printed.
fn make(tag: &str, kind: &str, mib: u64, args: &[&str]) -> (String, BTreeMap<String, String>) {
    let image =
        fs_ext4_test_support::temp_path!("fs_ext4_salvage_maps_{tag}_{}.img", std::process::id());
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
    let hashes = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (hash, path) = line.split_once("  ")?;
            Some((path.trim().to_string(), hash.to_string()))
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(hashes.len(), 4, "[{tag}] the kernel printed {hashes:?}");
    (image, hashes)
}

fn debugfs(request: &str, image: &str) -> String {
    let out = oracle("debugfs")
        .args(["-R", request])
        .arg(image)
        .judged()
        .clean("debugfs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The volume as the salvage reads see it.
struct Volume {
    source: FileDevice,
    sb: Superblock,
}

impl Volume {
    fn open(image: &str) -> Self {
        let source = FileDevice::open(image).unwrap();
        let sb = find_superblock(&source).unwrap().superblock.unwrap();
        Self { source, sb }
    }

    fn record(&self, ino: u32) -> InodeRecord {
        let descriptors = group_descriptors(&self.source, &self.sb, 0);
        read_inode(&self.source, &self.sb, &descriptors, ino).unwrap()
    }

    fn inode(&self, ino: u32) -> fs_ext4::inode::Inode {
        self.record(ino).inode.unwrap()
    }

    fn scan(&self, dir: u32) -> DirectoryScan {
        read_directory(&self.source, &self.sb, &self.record(dir))
    }

    /// `name` in directory `dir`, through the tolerant scan.
    fn child(&self, dir: u32, name: &str) -> u32 {
        self.scan(dir)
            .entries()
            .find(|e| e.live && e.name == name.as_bytes())
            .unwrap_or_else(|| panic!("no live entry {name} in inode {dir}"))
            .inode
    }

    fn lookup(&self, path: &str) -> u32 {
        path.split('/').fold(2, |dir, name| self.child(dir, name))
    }

    fn map(&self, ino: u32) -> BlockMap {
        map_inode(&self.source, &self.sb, &self.record(ino))
    }

    /// The bytes `map` names, holes as zeros, cut to `size`.
    fn contents(&self, map: &BlockMap, size: u64) -> Vec<u8> {
        let block_size = u64::from(self.sb.block_size());
        let mut out = vec![0u8; size as usize];
        for run in map.runs.iter().filter(|run| !run.uninitialized) {
            for step in 0..run.length {
                let start = (run.logical + step) * block_size;
                if start >= size {
                    break;
                }
                let end = (start + block_size).min(size);
                let mut buf = vec![0u8; (end - start) as usize];
                (&self.source as &dyn BlockRead)
                    .read_at((run.physical + step) * block_size, &mut buf)
                    .unwrap();
                out[start as usize..end as usize].copy_from_slice(&buf);
            }
        }
        out
    }
}

fn numbers(text: &str) -> BTreeSet<u64> {
    text.split_whitespace()
        .filter_map(|word| word.parse().ok())
        .collect()
}

/// The extent-tree and indirect blocks `debugfs stat` names:
/// `(IND):594`, `(DIND):595`, `(ETB0):1234`.
fn map_blocks_of(stat: &str) -> BTreeSet<u64> {
    stat.split([',', ' ', '\n'])
        .filter_map(|token| {
            let (label, block) = token.trim().split_once("):")?;
            let label = label.strip_prefix('(')?;
            let is_map = matches!(label, "IND" | "DIND" | "TIND") || label.starts_with("ETB");
            is_map.then(|| block.split('-').next()?.parse().ok())?
        })
        .collect()
}

/// `debugfs ls` / `ls -d` entries, live and deleted, by name: each is
/// `INODE (REC_LEN) NAME`, a deleted one with its inode in angle brackets.
/// A deleted record whose header was zeroed prints `<0> (36)` and no
/// name; it names nothing, so it is left out.
fn listed(text: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let words: Vec<&str> = text.split_whitespace().collect();
    let is_rec = |w: &str| {
        w.len() > 2
            && w.starts_with('(')
            && w.ends_with(')')
            && w[1..w.len() - 1].parse::<u64>().is_ok()
    };
    let inode = |w: &str| -> Option<(u64, bool)> {
        match w.strip_prefix('<').and_then(|w| w.strip_suffix('>')) {
            Some(n) => n.parse().ok().map(|n| (n, true)),
            None => w.parse().ok().map(|n| (n, false)),
        }
    };
    let mut live = BTreeSet::new();
    let mut deleted = BTreeSet::new();
    let mut i = 0;
    while i + 1 < words.len() {
        let Some((number, gone)) = inode(words[i]).filter(|_| is_rec(words[i + 1])) else {
            i += 1;
            continue;
        };
        let named = i + 2 < words.len()
            && !(inode(words[i + 2]).is_some() && words.get(i + 3).is_some_and(|w| is_rec(w)));
        if named {
            let name = words[i + 2].to_string();
            if !gone {
                live.insert(name);
            } else if number != 0 {
                deleted.insert(name);
            }
            i += 3;
        } else {
            i += 2;
        }
    }
    (live, deleted)
}

fn sha256(bytes: &[u8]) -> String {
    fs_ext4_test_support::sha256_hex(bytes)
}

fn compare_file(
    tag: &str,
    image: &str,
    volume: &Volume,
    path: &str,
    hashes: &BTreeMap<String, String>,
) {
    let ino = volume.lookup(path);
    let map = volume.map(ino);
    assert_eq!(map.end, MapEnd::Complete, "[{tag}] {path}: {map:?}");
    assert!(map.faults.is_empty(), "[{tag}] {path}: {map:?}");
    let ours: BTreeSet<u64> = map
        .runs
        .iter()
        .flat_map(|run| run.physical..run.physical + run.length)
        .chain(map.map_blocks.iter().copied())
        .collect();
    let theirs = numbers(
        debugfs(&format!("blocks <{ino}>"), image)
            .lines()
            .filter(|line| !line.starts_with("debugfs "))
            .collect::<Vec<_>>()
            .join(" ")
            .as_str(),
    );
    assert_eq!(ours, theirs, "[{tag}] {path}: blocks");
    let stat = debugfs(&format!("stat <{ino}>"), image);
    let ours: BTreeSet<u64> = map.map_blocks.iter().copied().collect();
    assert_eq!(
        ours,
        map_blocks_of(&stat),
        "[{tag}] {path}: map blocks\n{stat}"
    );
    let size = volume.inode(ino).size;
    assert_eq!(
        sha256(&volume.contents(&map, size)),
        hashes[path],
        "[{tag}] {path}: contents"
    );
}

fn compare(tag: &str, image: &str, hashes: &BTreeMap<String, String>) {
    let volume = Volume::open(image);
    for path in ["frag", "big", "d/f0", "d/f39"] {
        compare_file(tag, image, &volume, path, hashes);
    }
    assert!(
        !volume.map(volume.lookup("frag")).map_blocks.is_empty(),
        "[{tag}] the fragmented file has no map block below the inode"
    );
    for (path, target) in [("s1", "short".to_string()), ("s2", "long/".repeat(30))] {
        let ino = volume.lookup(path);
        let map = volume.map(ino);
        let size = volume.inode(ino).size;
        let got = match &map.inline {
            Some(bytes) => bytes.clone(),
            None => volume.contents(&map, size),
        };
        assert_eq!(got, target.as_bytes(), "[{tag}] {path}");
    }
    let dir = volume.lookup("d");
    let scan = volume.scan(dir);
    let ours_live: BTreeSet<String> = scan
        .entries()
        .filter(|e| e.live)
        .map(|e| String::from_utf8_lossy(&e.name).into_owned())
        .collect();
    let ours_deleted: BTreeSet<String> = scan
        .entries()
        .filter(|e| !e.live)
        .map(|e| String::from_utf8_lossy(&e.name).into_owned())
        .collect();
    let (live, _) = listed(&debugfs("ls /d", image));
    let (_, deleted) = listed(&debugfs("ls -d /d", image));
    assert_eq!(ours_live, live, "[{tag}] live entries of /d");
    assert_eq!(ours_deleted, deleted, "[{tag}] deleted entries of /d");
    assert_eq!(live.len(), 40, "[{tag}] . and .. and 38 files");
    for (_, _, block) in &scan.blocks {
        assert!(block.faults.is_empty(), "[{tag}] {block:?}");
    }
}

fn write_at(image: &str, offset: u64, bytes: &[u8]) {
    let file = std::fs::OpenOptions::new().write(true).open(image).unwrap();
    file.write_all_at(bytes, offset).unwrap();
}

/// A copy of `image`, beside it, to damage one way.
fn copy_of(image: &str, what: &str) -> String {
    let copy = format!("{image}.{what}");
    std::fs::copy(image, &copy).unwrap();
    copy
}

/// The fragmented file's first map block zeroed: e2fsck reports it, and
/// the map stops there and says why, keeping what the root still names.
fn map_block_zeroed(tag: &str, image: &str) {
    let image = &copy_of(image, "map");
    let volume = Volume::open(image);
    let block_size = u64::from(volume.sb.block_size());
    let frag = volume.lookup("frag");
    let pristine = volume.map(frag);
    let first_map_block = pristine.map_blocks[0];
    drop(volume);
    write_at(
        image,
        first_map_block * block_size,
        &vec![0u8; block_size as usize],
    );
    oracle("e2fsck")
        .args(["-fn"])
        .arg(image)
        .judged()
        .findings(&format!("{tag}: zeroed map block"));
    let volume = Volume::open(image);
    let map = volume.map(frag);
    assert_ne!(map.end, MapEnd::Complete, "[{tag}] {map:?}");
    assert!(
        map.faults
            .iter()
            .any(|(_, f)| f.kind == DamageKind::Signature)
            || map.end != MapEnd::Complete,
        "[{tag}] {map:?}"
    );
    assert!(map.mapped_blocks() < pristine.mapped_blocks(), "[{tag}]");
    let _ = std::fs::remove_file(image);
}

/// `d/f30`'s record length broken: e2fsck, repairing a copy, reports the
/// directory corrupted (`-n` stops at the question it may not answer), and
/// the scan keeps every other entry and names the broken record.
fn record_length_broken(tag: &str, image: &str) {
    let image = &copy_of(image, "dir");
    let volume = Volume::open(image);
    let dir = volume.lookup("d");
    let scan = volume.scan(dir);
    let live_before = scan.entries().filter(|e| e.live).count();
    let (target_block, target) = scan
        .blocks
        .iter()
        .find_map(|(_, at, block)| {
            let entry = block.entries.iter().find(|e| e.name == b"f30")?;
            Some((at.offset, entry.offset as u64))
        })
        .unwrap();
    drop(volume);
    write_at(image, target_block + target + 4, &3u16.to_le_bytes());

    let repaired = copy_of(image, "fsck");
    let said = oracle("e2fsck")
        .args(["-fy"])
        .arg(&repaired)
        .judged()
        .repaired(&format!("{tag}: broken record length"));
    assert!(
        said.contains("directory corrupted"),
        "[{tag}] e2fsck did not report the directory:\n{said}"
    );
    let _ = std::fs::remove_file(&repaired);

    let volume = Volume::open(image);
    let scan = volume.scan(dir);
    let live_after: Vec<&[u8]> = scan
        .entries()
        .filter(|e| e.live)
        .map(|e| e.name.as_slice())
        .collect();
    assert_eq!(live_after.len(), live_before - 1, "[{tag}] {live_after:?}");
    assert!(!live_after.contains(&&b"f30"[..]), "[{tag}]");
    assert!(
        scan.blocks
            .iter()
            .flat_map(|(_, _, b)| b.faults.iter())
            .any(|(at, f)| *at as u64 == target && f.kind == DamageKind::Malformed),
        "[{tag}] the broken record is not reported"
    );
    let _ = std::fs::remove_file(image);
}

fn check(tag: &str, kind: &str, mib: u64, args: &[&str]) {
    let (image, hashes) = make(tag, kind, mib, args);
    compare(tag, &image, &hashes);
    map_block_zeroed(tag, &image);
    record_length_broken(tag, &image);
    let _ = std::fs::remove_file(&image);
}

#[test]
fn ext2_volume_with_1k_blocks() {
    check("ext2-1k", "ext2", 32, &["-b", "1024"]);
}

#[test]
fn ext3_volume_with_4k_blocks() {
    check("ext3-4k", "ext3", 64, &["-b", "4096"]);
}

#[test]
fn two_k_ext4_volume() {
    check("ext4-2k", "ext4", 64, &["-b", "2048"]);
}

#[test]
fn four_k_ext4_volume() {
    check("ext4-4k", "ext4", 64, &["-b", "4096"]);
}

/// An inline directory (its entries in the inode) and a hashed one (its
/// first block an index whose record lengths cover the hash tree): the
/// scan lists the entries `debugfs ls` does, and finds no deleted entry in
/// the index's bytes where `debugfs ls -d` finds none.
#[test]
fn inline_and_hashed_directories() {
    let tag = "inline-htree";
    let image =
        fs_ext4_test_support::temp_path!("fs_ext4_salvage_dirs_{tag}_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(64 << 20))
        .unwrap();
    let out = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-O", "inline_data"])
        .arg(&image)
        .output();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    guest_kernel_write(
        &image,
        r#"
mkdir "$MNT/small" "$MNT/big"
touch "$MNT/small/a" "$MNT/small/b"
for i in $(seq 1 400); do touch "$MNT/big/a-file-with-a-long-name-$i"; done
sync
"#,
    );
    let volume = Volume::open(&image);
    let small = volume.lookup("small");
    assert!(
        volume.inode(small).has_inline_data(),
        "[{tag}] /small is not inline"
    );
    let big = volume.lookup("big");
    assert_ne!(
        volume.inode(big).flags & 0x1000,
        0,
        "[{tag}] /big is not hashed"
    );
    for (path, ino) in [("/small", small), ("/big", big)] {
        let scan = volume.scan(ino);
        let names = |live: bool| -> BTreeSet<String> {
            scan.entries()
                .filter(|e| e.live == live)
                .map(|e| String::from_utf8_lossy(&e.name).into_owned())
                .collect()
        };
        let (live, _) = listed(&debugfs(&format!("ls {path}"), &image));
        // debugfs also reads names out of a hashed directory's index bytes
        // and prints them escaped (`\x8e...`); the scan refuses a name no
        // entry could hold, so only names of printable characters compare.
        let (_, deleted) = listed(&debugfs(&format!("ls -d {path}"), &image));
        let deleted: BTreeSet<String> = deleted
            .into_iter()
            .filter(|name| !name.contains("\\x") && name.chars().all(|c| c.is_ascii_graphic()))
            .collect();
        let mut ours = names(true);
        if let Some(parent) = scan.inline_parent {
            assert_eq!(parent, 2, "[{tag}] {path}");
            // An inline directory keeps `.` and `..` implicit; debugfs
            // lists them.
            ours.insert(".".to_string());
            ours.insert("..".to_string());
        }
        let only_ours: Vec<_> = ours.difference(&live).collect();
        let only_theirs: Vec<_> = live.difference(&ours).collect();
        assert!(
            only_ours.is_empty() && only_theirs.is_empty(),
            "[{tag}] {path}: only ours {only_ours:?}, only debugfs {only_theirs:?}; first block {:?}",
            scan.blocks.first().map(|(_, at, b)| (at, &b.faults, b.entries.iter().take(3).collect::<Vec<_>>()))
        );
        assert_eq!(names(false), deleted, "[{tag}] {path} deleted");
    }
    let _ = std::fs::remove_file(&image);
}
