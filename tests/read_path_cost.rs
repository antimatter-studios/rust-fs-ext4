//! What a read costs, in calls to the device (#68).
//!
//! The number a cache moves is calls to the device, and it is
//! deterministic: the same image walked the same way makes the same calls,
//! so a change that makes the driver ask for more is visible. Wall time is
//! printed beside it and asserted on by nothing.
//!
//! Four shapes, because they cost differently:
//!
//! - **mount**: superblock, group descriptors, journal.
//! - **walk**: every directory in the tree listed.
//! - **stat**: every path resolved from the root and its inode read.
//! - **read**: every regular file's contents.
//!
//! Each is measured on a mount with no clean cache (`mount_with_cache(.., 0)`)
//! and on one with the default [`fs_ext4::fs::DEFAULT_CACHE_BLOCKS`], through
//! `am-fs-core`'s `CountingDevice` below the cache, as the sibling drivers
//! measure. The tree is built here with `mkfs.ext4 -d` to a fixed recipe, so
//! the figures in `docs/read-path-cost.md` can be reproduced. Fails when
//! the harness VM the e2fsprogs tools run in is unreachable.

#![cfg(unix)]

use fs_core::{BlockRead, CountingDevice, FileDevice};
use fs_ext4::fs::{Filesystem, DEFAULT_CACHE_BLOCKS};
use std::sync::Arc;
use std::time::Instant;

/// The block size the image is made with.
const BLOCK: u64 = 4096;

/// ext4's `BlockDevice` over the counter, read-only.
///
/// It also counts the requests that were not exactly one block at a block
/// boundary. The counter only has totals, and bytes over calls is an
/// average: requests of mixed lengths, or a 4 KiB request straddling two
/// blocks, give the same ratio as one aligned block each.
struct Counted(Arc<CountingDevice>, std::sync::atomic::AtomicU64);

impl fs_ext4::block_io::BlockDevice for Counted {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_ext4::Result<()> {
        if !offset.is_multiple_of(BLOCK) || buf.len() as u64 != BLOCK {
            self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.0
            .read_at(offset, buf)
            .map_err(|e| fs_ext4::Error::Io(std::io::Error::other(e.to_string())))
    }
    fn size_bytes(&self) -> u64 {
        self.0.size_bytes()
    }
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> fs_ext4::Result<()> {
        Err(fs_ext4::Error::ReadOnly)
    }
}

struct Cost {
    reads: u64,
    bytes: u64,
    micros: u128,
    items: usize,
}

struct Pass {
    mount: Cost,
    walk: Cost,
    stat: Cost,
    read: Cost,
}

/// The measured tree: ten directories of 200 files of assorted sizes, three
/// levels of nesting, and one directory of 3000 names indexed by `e2fsck -D`.
fn build_image() -> String {
    let mkfs = "mkfs.ext4";
    let e2fsck = "e2fsck";
    let root = fs_ext4_test_support::temp_path!("fs_ext4_read_cost_{}", std::process::id());
    for d in 0..10 {
        let dir = format!("{root}/d{d}");
        std::fs::create_dir_all(format!("{dir}/a/b")).unwrap();
        for f in 0..200usize {
            let len = (f * 7919 + d * 131) % 20_000;
            let bytes: Vec<u8> = (0..len).map(|i| (i * 31 + f) as u8).collect();
            std::fs::write(format!("{dir}/f{f:03}"), bytes).unwrap();
        }
        std::fs::write(format!("{dir}/a/b/deep"), b"deep").unwrap();
    }
    std::fs::create_dir_all(format!("{root}/many")).unwrap();
    for i in 0..3000 {
        std::fs::write(format!("{root}/many/entry_{i:05}"), b"").unwrap();
    }
    let image = format!("{root}.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(128 * 1024 * 1024))
        .unwrap();
    let ok = |call: fs_ext4_test_support::Oracle| call.output().status.code();
    assert_eq!(
        ok(fs_ext4_test_support::oracle(mkfs)
            .args(["-q", "-F", "-b", "4096", "-d", &root, &image])),
        Some(0)
    );
    fs_ext4_test_support::oracle(e2fsck)
        .args(["-fyD", &image])
        .judged()
        .repaired("e2fsck -fyD");
    let _ = std::fs::remove_dir_all(&root);
    image
}

fn entries(fs: &Filesystem, ino: u32) -> Vec<(Vec<u8>, u32)> {
    let (inode, _) = fs.read_inode_verified(ino).unwrap();
    let data = fs_ext4::file_io::read_all(fs, &inode).unwrap();
    let bs = fs.sb.block_size() as usize;
    data.chunks(bs)
        .flat_map(|b| {
            // A block that does not parse is a failure, not an empty block:
            // skipping it would shrink every later pass's work and still pass.
            fs_ext4::dir::parse_block(b, true)
                .unwrap_or_else(|e| panic!("a block of directory {ino} does not parse: {e:?}"))
        })
        .filter(|e| e.inode != 0 && e.name != b"." && e.name != b"..")
        .map(|e| (e.name, e.inode))
        .collect()
}

/// Every path under `at`, with whether it is a directory.
fn walk(fs: &Filesystem, at: &str, ino: u32, out: &mut Vec<(String, u32, bool)>) {
    for (name, child) in entries(fs, ino) {
        let path = format!(
            "{}/{}",
            at.trim_end_matches('/'),
            String::from_utf8_lossy(&name)
        );
        let (inode, _) = fs.read_inode_verified(child).unwrap();
        let is_dir = inode.is_dir();
        out.push((path.clone(), child, is_dir));
        if is_dir {
            walk(fs, &path, child, out);
        }
    }
}

fn measure(counting: &CountingDevice, items: usize, body: impl FnOnce()) -> Cost {
    counting.reset();
    let start = Instant::now();
    body();
    Cost {
        reads: counting.reads(),
        bytes: counting.bytes(),
        micros: start.elapsed().as_micros(),
        items,
    }
}

fn report(what: &str, c: &Cost) {
    eprintln!(
        "{what:<6} {:>7} reads {:>11} bytes {:>9} us  over {:>5} items",
        c.reads, c.bytes, c.micros, c.items
    );
}

fn measure_pass(image: &str, blocks: usize) -> Pass {
    let counting = Arc::new(CountingDevice::new(Arc::new(
        FileDevice::open(image).unwrap(),
    )));
    let device = Arc::new(Counted(counting.clone(), Default::default()));
    let mut fs = None;
    let mount = measure(&counting, 1, || {
        fs = Some(Filesystem::mount_with_cache(device.clone(), blocks).expect("mount"));
    });
    let fs = fs.unwrap();

    let mut paths = Vec::new();
    let mut dirs = 0;
    let walk_cost = measure(&counting, 0, || {
        walk(&fs, "/", 2, &mut paths);
        dirs = paths.iter().filter(|p| p.2).count() + 1;
    });
    let walk_cost = Cost {
        items: dirs,
        ..walk_cost
    };

    let stat = measure(&counting, paths.len(), || {
        for (path, ino, _) in &paths {
            let mut reader = |i: u32| fs.read_inode_verified(i).map(|(inode, _)| inode);
            let found = fs_ext4::path::lookup_with_csum(
                fs.dev.as_ref(),
                &fs.sb,
                &mut reader,
                path,
                &fs.csum,
            )
            .unwrap();
            assert_eq!(found, *ino, "{path}");
            fs.read_inode_verified(found).unwrap();
        }
    });

    let files: Vec<u32> = paths.iter().filter(|p| !p.2).map(|p| p.1).collect();
    // The recipe's own counts, so a walk that came up short fails here
    // instead of making every later figure smaller: ten directories with
    // `a` and `a/b` each, `many`, `lost+found` and the root; 200 files in
    // each of the ten, a `deep` under each, and 3000 in `many`.
    assert_eq!(dirs, 33, "directories walked");
    assert_eq!(files.len(), 5010, "files found");
    assert_eq!(paths.len(), 5042, "paths found");
    let read = measure(&counting, files.len(), || {
        for ino in &files {
            let (inode, raw) = fs.read_inode_verified(*ino).unwrap();
            let mut out = vec![0u8; inode.size as usize];
            fs_ext4::file_io::read_with_raw_verified(
                &fs, &inode, &raw, *ino, 0, inode.size, &mut out,
            )
            .unwrap();
        }
    });

    for (what, c) in [
        ("mount", &mount),
        ("walk", &walk_cost),
        ("stat", &stat),
        ("read", &read),
    ] {
        report(what, c);
    }
    let odd = device.1.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!("requests that were not one aligned {BLOCK}-byte block: {odd}");
    Pass {
        mount,
        walk: walk_cost,
        stat,
        read,
    }
}

#[test]
fn what_a_read_costs_in_calls_to_the_device() {
    let image = build_image();
    eprintln!("--- no clean cache ---");
    let uncached = measure_pass(&image, 0);
    eprintln!("--- default cache ({DEFAULT_CACHE_BLOCKS} blocks) ---");
    let cached = measure_pass(&image, DEFAULT_CACHE_BLOCKS);
    eprintln!(
        "--- four times the default ({} blocks) ---",
        4 * DEFAULT_CACHE_BLOCKS
    );
    let larger = measure_pass(&image, 4 * DEFAULT_CACHE_BLOCKS);

    // The uncached pass must reach the device, or the counter is not wired
    // below the mount and every figure is fiction.
    for (what, c) in [
        ("mount", &uncached.mount),
        ("walk", &uncached.walk),
        ("stat", &uncached.stat),
        ("read", &uncached.read),
    ] {
        assert!(c.items > 0, "{what}: measured nothing");
        assert!(c.reads > 0, "{what}: no call reached the device");
    }
    // A cache may fetch whole blocks where the uncached path asked for an
    // inode's 256 bytes, so bytes can rise; calls must not.
    for (what, un, ca) in [
        ("walk", &uncached.walk, &cached.walk),
        ("stat", &uncached.stat, &cached.stat),
        ("read", &uncached.read, &cached.read),
    ] {
        assert_eq!(ca.items, un.items, "{what}: the passes did different work");
        assert!(
            ca.reads <= un.reads,
            "{what}: the cache made it ask for more ({} vs {})",
            ca.reads,
            un.reads
        );
    }
    // An LRU cache is a stack algorithm: a larger one holds a superset of
    // what a smaller one holds, so it cannot need more calls.
    for (what, small, big) in [
        ("walk", &cached.walk, &larger.walk),
        ("stat", &cached.stat, &larger.stat),
        ("read", &cached.read, &larger.read),
    ] {
        assert!(
            big.reads <= small.reads,
            "{what}: a larger cache asked for more ({} vs {})",
            big.reads,
            small.reads
        );
    }
    let _ = std::fs::remove_file(&image);
}
