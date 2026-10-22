//! The inode-addressed entry points (#372) behave exactly as their
//! path-addressed twins, and a handle to a freed inode is refused as stale.
//!
//! A handle-based host names a file by its inode number and a mutation by a
//! `(directory inode, name)` pair; it never holds a path. Each such entry
//! point shares its implementation with the path function it mirrors, which
//! only resolves the path and delegates. That is checked here the strong
//! way: two copies of one freshly formatted volume, the same operation
//! sequence applied to one through paths and to the other through inodes,
//! and after every step the two results must agree and the two images must
//! be byte-identical. A deterministic runtime makes timestamps and inode
//! generations reproducible, so any divergence in what was written shows.
//!
//! e2fsck's view of an image written through these entry points is in
//! `tests/inode_api_e2fsck.rs`.

use fs_ext4::block_io::BlockDevice;
use fs_ext4::error::Result;
use fs_ext4::features::FsFlavor;
use fs_ext4::mkfs::format_filesystem_with_flavor;
use fs_ext4::runtime::Runtime;
use fs_ext4::{Error, Filesystem, InodeRef};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

const SIZE: u64 = 8 * 1024 * 1024;
const ROOT: u32 = 2;

struct MemDev {
    bytes: Mutex<Vec<u8>>,
}

impl MemDev {
    fn with(bytes: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            bytes: Mutex::new(bytes),
        })
    }
    fn snapshot(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }
    /// The first byte at which two devices differ, if any.
    fn first_difference(&self, other: &MemDev) -> Option<usize> {
        let (a, b) = (self.bytes.lock().unwrap(), other.bytes.lock().unwrap());
        if *a == *b {
            return None;
        }
        a.iter().zip(b.iter()).position(|(x, y)| x != y)
    }
}

impl BlockDevice for MemDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let b = self.bytes.lock().unwrap();
        let start = offset as usize;
        buf.copy_from_slice(&b[start..start + buf.len()]);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        SIZE
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut b = self.bytes.lock().unwrap();
        let start = offset as usize;
        b[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

/// Fixed clock, and generations drawn from a per-volume counter: the two
/// twins allocate the same inodes in the same order, so they must stamp the
/// same generations too.
struct Deterministic(AtomicU32);

impl Runtime for Deterministic {
    fn now_unix_seconds(&self) -> i64 {
        1_700_000_000
    }
    fn next_inode_generation(&self) -> u32 {
        self.0
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9)
    }
}

fn formatted(flavor: FsFlavor) -> Vec<u8> {
    let dev = MemDev::with(vec![0u8; SIZE as usize]);
    format_filesystem_with_flavor(dev.as_ref(), None, Some([0x37; 16]), SIZE, 1024, flavor)
        .unwrap();
    dev.snapshot()
}

fn mount(bytes: Vec<u8>) -> (Arc<MemDev>, Filesystem) {
    let dev = MemDev::with(bytes);
    let fs =
        Filesystem::mount_with_runtime(dev.clone(), Arc::new(Deterministic(AtomicU32::new(1))))
            .unwrap();
    (dev, fs)
}

/// Resolve `path` through `lookup_at` alone, one component at a time, the
/// way a handle-based host walks: a symlink before the final component is
/// read with `read_link` and its target walked in its place, as the
/// kernel's own walk does (#531).
fn walk(fs: &Filesystem, path: &str) -> Result<u32> {
    walk_following(fs, path, false)
}

/// [`walk`], following a symlink as the final component too when
/// `follow_last` is set, as the kernel walks a name's parent (#564).
fn walk_following(fs: &Filesystem, path: &str, follow_last: bool) -> Result<u32> {
    let push = |pending: &mut Vec<Vec<u8>>, p: &[u8]| {
        let parts = p.split(|&b| b == b'/').filter(|c| !c.is_empty());
        pending.extend(parts.rev().map(<[u8]>::to_vec));
    };
    let mut pending = Vec::new();
    push(&mut pending, path.as_bytes());
    let (mut ino, mut followed) = (ROOT, 0);
    while let Some(name) = pending.pop() {
        let child = fs.lookup_at(ino, &name)?;
        if (pending.is_empty() && !follow_last) || !fs.stat_ino(child)?.is_symlink() {
            ino = child;
            continue;
        }
        followed += 1;
        if followed > fs_ext4::fs::MAX_SYMLINKS {
            return Err(Error::SymlinkLoop);
        }
        let target = fs.read_link(child)?;
        if target.is_empty() {
            return Err(Error::NotFound);
        }
        if target[0] == b'/' {
            ino = ROOT;
        }
        push(&mut pending, &target);
    }
    Ok(ino)
}

/// `(parent inode, final name)` for `path`, resolved by inode.
fn walk_parent<'a>(fs: &Filesystem, path: &'a str) -> Result<(u32, &'a [u8])> {
    let cut = path.rfind('/').unwrap();
    Ok((
        walk_following(fs, &path[..cut], true)?,
        &path.as_bytes()[cut + 1..],
    ))
}

#[derive(Clone, Debug)]
enum Op {
    Create(String),
    Mkdir(String),
    Fifo(String),
    Symlink(String, String),
    Link(String, String),
    Unlink(String),
    Rmdir(String),
    Rename(String, String, bool),
    Pwrite(String, u64, usize),
    Chmod(String, u16),
    Chown(String, u32, u32),
    Utimens(String, i64),
}

/// What an operation came to: its value, or its errno.
fn outcome<T: std::fmt::Debug>(r: Result<T>) -> std::result::Result<String, i32> {
    r.map(|v| format!("{v:?}")).map_err(|e| e.to_errno())
}

fn payload(len: usize, seed: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + seed) as u8).collect()
}

fn by_path(fs: &Filesystem, op: &Op) -> std::result::Result<String, i32> {
    match op {
        Op::Create(p) => outcome(fs.apply_create(p, 0o644)),
        Op::Mkdir(p) => outcome(fs.apply_mkdir(p, 0o755)),
        Op::Fifo(p) => outcome(fs.apply_mknod(p, 0o010644, 0, 0)),
        Op::Symlink(t, p) => outcome(fs.apply_symlink(t, p)),
        Op::Link(s, d) => outcome(fs.apply_link(s, d)),
        Op::Unlink(p) => outcome(fs.apply_unlink(p)),
        Op::Rmdir(p) => outcome(fs.apply_rmdir(p)),
        Op::Rename(s, d, r) => outcome(fs.apply_rename(s, d, *r)),
        Op::Pwrite(p, off, len) => outcome(fs.apply_pwrite(p, *off, &payload(*len, *off as usize))),
        Op::Chmod(p, m) => outcome(fs.apply_chmod(p, *m)),
        Op::Chown(p, u, g) => outcome(fs.apply_chown(p, *u, *g)),
        Op::Utimens(p, t) => outcome(fs.apply_utimens(p, *t, 5, *t + 1, 7)),
    }
}

fn by_inode(fs: &Filesystem, op: &Op) -> std::result::Result<String, i32> {
    match op {
        Op::Create(p) => {
            outcome(walk_parent(fs, p).and_then(|(d, n)| fs.apply_create_at(d, n, 0o644)))
        }
        Op::Mkdir(p) => {
            outcome(walk_parent(fs, p).and_then(|(d, n)| fs.apply_mkdir_at(d, n, 0o755)))
        }
        Op::Fifo(p) => {
            outcome(walk_parent(fs, p).and_then(|(d, n)| fs.apply_mknod_at(d, n, 0o010644, 0, 0)))
        }
        Op::Symlink(t, p) => {
            outcome(walk_parent(fs, p).and_then(|(d, n)| fs.apply_symlink_at(d, n, t.as_bytes())))
        }
        Op::Link(s, d) => outcome(walk(fs, s).and_then(|ino| {
            // The path twin refuses a directory before it resolves the
            // destination; a host holding both handles never has to choose.
            if fs.stat_ino(ino)?.is_dir() {
                return Err(Error::IsADirectory);
            }
            let (dir, name) = walk_parent(fs, d)?;
            fs.apply_link_at(ino, dir, name)
        })),
        Op::Unlink(p) => outcome(walk_parent(fs, p).and_then(|(d, n)| fs.apply_unlink_at(d, n))),
        Op::Rmdir(p) => outcome(walk_parent(fs, p).and_then(|(d, n)| fs.apply_rmdir_at(d, n))),
        Op::Rename(s, d, r) => outcome(walk_parent(fs, s).and_then(|(sd, sn)| {
            let (dd, dn) = walk_parent(fs, d)?;
            fs.apply_rename_at(sd, sn, dd, dn, *r)
        })),
        Op::Pwrite(p, off, len) => outcome(
            walk(fs, p).and_then(|i| fs.apply_pwrite_ino(i, *off, &payload(*len, *off as usize))),
        ),
        Op::Chmod(p, m) => outcome(walk(fs, p).and_then(|i| fs.apply_chmod_ino(i, *m))),
        Op::Chown(p, u, g) => outcome(walk(fs, p).and_then(|i| fs.apply_chown_ino(i, *u, *g))),
        Op::Utimens(p, t) => {
            outcome(walk(fs, p).and_then(|i| fs.apply_utimens_ino(i, *t, 5, *t + 1, 7)))
        }
    }
}

/// Apply `ops` to two copies of one volume, one by path and one by inode,
/// and require the same outcome and the same bytes after every step.
fn twins_agree(flavor: FsFlavor, ops: &[Op]) -> usize {
    let image = formatted(flavor);
    let (path_dev, path_fs) = mount(image.clone());
    let (ino_dev, ino_fs) = mount(image);
    // A volume's first write marks it not clean. Take that write on both
    // now, so a step refused before it reaches the engine on one side (an
    // inode walk that fails) cannot leave the two differing by that flag.
    for fs in [&path_fs, &ino_fs] {
        fs.apply_chmod("/", 0o755).unwrap();
    }
    let mut succeeded = 0;
    for (step, op) in ops.iter().enumerate() {
        let a = by_path(&path_fs, op);
        let b = by_inode(&ino_fs, op);
        assert_eq!(
            a, b,
            "{flavor:?} step {step} {op:?}: path and inode outcomes differ"
        );
        if a.is_ok() {
            succeeded += 1;
        }
        if let Some(first) = path_dev.first_difference(&ino_dev) {
            panic!(
                "{flavor:?} step {step} {op:?}: images differ, first at byte {first} \
                 (block {} at 1 KiB)",
                first / 1024
            );
        }
    }
    succeeded
}

fn p(s: &str) -> String {
    s.to_string()
}

/// A scripted sequence that reaches every entry point's success path and
/// the usual refusals, including a directory grown past its first block.
fn scripted() -> Vec<Op> {
    let mut ops = vec![
        Op::Mkdir(p("/a")),
        Op::Mkdir(p("/a/b")),
        Op::Mkdir(p("/c")),
        Op::Create(p("/a/f")),
        Op::Create(p("/a/f")), // EEXIST
        Op::Pwrite(p("/a/f"), 0, 5000),
        Op::Pwrite(p("/a/f"), 9000, 100),
        Op::Link(p("/a/f"), p("/c/g")),
        Op::Link(p("/a"), p("/c/dirlink")), // EISDIR
        Op::Symlink(p("short"), p("/a/s")),
        Op::Symlink("x".repeat(200), p("/a/long")),
        Op::Fifo(p("/c/fifo")),
        Op::Chmod(p("/a/f"), 0o600),
        Op::Chown(p("/c/g"), 1000, u32::MAX),
        Op::Utimens(p("/a/b"), 1_600_000_000),
        Op::Utimens(p("/a/b"), 99_999_999_999), // EINVAL: out of range
        Op::Unlink(p("/a/b")),                  // EISDIR
        Op::Rmdir(p("/a")),                     // ENOTEMPTY
        Op::Rename(p("/a"), p("/a/b/x"), false), // EINVAL: into itself
        Op::Rename(p("/a/b"), p("/c/b"), false),
        Op::Rename(p("/a"), p("/d"), false),
        Op::Rename(p("/c/fifo"), p("/d/f"), false), // EEXIST
        Op::Rename(p("/c/fifo"), p("/d/f"), true),  // replace; f keeps /c/g
        Op::Rename(p("/d/s"), p("/d/s"), false),    // same name: no-op
        Op::Rename(p("/d/none"), p("/d/none"), false), // ENOENT
        Op::Create(p("/nope/x")),                   // ENOENT
        Op::Create(p("/c/g/x")),                    // ENOTDIR
        Op::Pwrite(p("/c"), 0, 1),                  // EINVAL: a directory
        Op::Unlink(p("/c/g")),
        Op::Rmdir(p("/c/b")),
        Op::Mkdir(p("/big")),
    ];
    // Enough names to grow /big past one 1 KiB block several times over.
    for i in 0..60 {
        ops.push(Op::Create(format!("/big/entry-with-a-longish-name-{i:03}")));
    }
    for i in (0..60).step_by(3) {
        ops.push(Op::Unlink(format!("/big/entry-with-a-longish-name-{i:03}")));
    }
    ops.push(Op::Rename(
        p("/big/entry-with-a-longish-name-001"),
        p("/d/moved"),
        false,
    ));
    ops
}

#[test]
fn a_scripted_sequence_writes_the_same_bytes_both_ways() {
    for flavor in [FsFlavor::Ext4, FsFlavor::Ext3, FsFlavor::Ext2] {
        let succeeded = twins_agree(flavor, &scripted());
        // Guard against a sequence that proves nothing because everything
        // failed the same way. Ext2/3 refuse pwrite on a block-mapped file,
        // both ways alike, so they succeed a little less.
        let floor = if matches!(flavor, FsFlavor::Ext4) {
            100
        } else {
            95
        };
        assert!(
            succeeded >= floor,
            "{flavor:?}: only {succeeded} steps succeeded"
        );
    }
}

/// A small deterministic generator, so a failure names its seed and step.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as usize) % n
    }
}

fn random_ops(seed: u64, count: usize) -> Vec<Op> {
    const PARENTS: [&str; 6] = ["", "", "/a", "/b", "/a/b", "/c/a"];
    const NAMES: [&str; 4] = ["a", "b", "c", "d"];
    let mut rng = Lcg(seed);
    // Every parent in the pool exists at the start, so the random steps
    // mostly act rather than mostly fail to resolve.
    let mut ops: Vec<Op> = ["/a", "/b", "/c", "/a/b", "/c/a"]
        .iter()
        .map(|d| Op::Mkdir(p(d)))
        .collect();
    let path = |rng: &mut Lcg| {
        format!(
            "{}/{}",
            PARENTS[rng.next(PARENTS.len())],
            NAMES[rng.next(NAMES.len())]
        )
    };
    ops.extend((0..count).map(|_| match rng.next(12) {
        0 | 1 => Op::Mkdir(path(&mut rng)),
        2 | 3 => Op::Create(path(&mut rng)),
        4 => Op::Symlink("t".repeat(1 + rng.next(120)), path(&mut rng)),
        5 => Op::Link(path(&mut rng), path(&mut rng)),
        6 => Op::Unlink(path(&mut rng)),
        7 => Op::Rmdir(path(&mut rng)),
        8 => {
            let replace = rng.next(2) == 0;
            Op::Rename(path(&mut rng), path(&mut rng), replace)
        }
        9 => {
            let off = rng.next(3) as u64 * 700;
            Op::Pwrite(path(&mut rng), off, 1 + rng.next(3000))
        }
        10 => Op::Chmod(path(&mut rng), rng.next(0o7777) as u16),
        _ => Op::Fifo(path(&mut rng)),
    }));
    ops
}

#[test]
fn random_sequences_write_the_same_bytes_both_ways() {
    let mut succeeded = 0;
    for seed in 1..=12u64 {
        succeeded += twins_agree(FsFlavor::Ext4, &random_ops(seed, 80));
    }
    // A generator that only ever produced refusals would agree vacuously.
    assert!(succeeded >= 300, "only {succeeded} random steps succeeded");
}

#[test]
fn the_read_side_matches_the_path_side() {
    let (_dev, fs) = mount(formatted(FsFlavor::Ext4));
    fs.apply_mkdir("/d", 0o755).unwrap();
    let f = fs.apply_create("/d/f", 0o644).unwrap();
    fs.apply_pwrite("/d/f", 3, b"hello, inode").unwrap();
    let s = fs.apply_symlink("../d/f", "/d/s").unwrap();

    let d = fs.lookup_at(ROOT, b"d").unwrap();
    assert_eq!(fs.lookup_at(d, b"f").unwrap(), f);
    assert_eq!(fs.lookup_at(d, b"s").unwrap(), s);
    assert_eq!(fs.lookup_at(d, b"..").unwrap(), ROOT);
    assert_eq!(
        fs.lookup_at(d, b"missing").unwrap_err().to_errno(),
        Error::NotFound.to_errno()
    );

    let (by_number, _) = fs.read_inode_verified(f).unwrap();
    let live = fs.stat_ino(f).unwrap();
    assert_eq!(format!("{live:?}"), format!("{by_number:?}"));

    let mut out = vec![0u8; 64];
    let n = fs.read_ino(f, 0, &mut out).unwrap();
    assert_eq!(&out[..n], b"\0\0\0hello, inode");
    let n = fs.read_ino(f, 5, &mut out[..4]).unwrap();
    assert_eq!(&out[..n], b"llo,");
    assert_eq!(fs.read_link_ino(s).unwrap(), b"../d/f");
    assert_eq!(fs.read_link_ino(s).unwrap(), fs.read_link(s).unwrap());

    let mut names: Vec<(Vec<u8>, u32)> = fs
        .read_dir_ino(d)
        .unwrap()
        .into_iter()
        .map(|e| (e.name, e.inode))
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            (b".".to_vec(), d),
            (b"..".to_vec(), ROOT),
            (b"f".to_vec(), f),
            (b"s".to_vec(), s),
        ]
    );
    assert_eq!(
        fs.read_dir_ino(f).unwrap_err().to_errno(),
        Error::NotADirectory.to_errno()
    );
}

fn errno_of<T: std::fmt::Debug>(r: Result<T>) -> i32 {
    r.expect_err("expected a refusal").to_errno()
}

fn stale() -> i32 {
    Error::Stale.to_errno()
}

#[test]
fn a_file_size_beyond_the_mapping_limit_is_efbig() {
    assert_eq!(Error::FileTooLarge.to_errno(), fs_ext4::error::errno::EFBIG);
}

#[test]
fn a_freed_and_reused_inode_is_stale_under_its_old_generation() {
    let (dev, fs) = mount(formatted(FsFlavor::Ext4));
    let f = fs.apply_create_at(ROOT, b"f", 0o644).unwrap();
    let f_gen = fs.stat_ino(f).unwrap().generation;
    let old = InodeRef::new(f, f_gen);
    fs.apply_pwrite_ino(old, 0, b"first").unwrap();

    // Freed: refused whether or not the caller names a generation.
    fs.apply_unlink_at(ROOT, b"f").unwrap();
    assert_eq!(errno_of(fs.stat_ino(old)), stale());
    assert_eq!(errno_of(fs.stat_ino(f)), stale());

    // Reused: the slot is live again, under a new generation.
    let g = fs.apply_create_at(ROOT, b"g", 0o644).unwrap();
    assert_eq!(
        g, f,
        "the allocator must reuse the freed slot for this test to mean anything"
    );
    let g_gen = fs.stat_ino(g).unwrap().generation;
    assert_ne!(g_gen, f_gen);

    let before = dev.snapshot();
    assert_eq!(errno_of(fs.stat_ino(old)), stale());
    assert_eq!(errno_of(fs.read_ino(old, 0, &mut [0u8; 8])), stale());
    assert_eq!(errno_of(fs.apply_pwrite_ino(old, 0, b"clobber")), stale());
    assert_eq!(errno_of(fs.apply_truncate_ino(old, 0)), stale());
    assert_eq!(errno_of(fs.apply_chmod_ino(old, 0o777)), stale());
    assert_eq!(errno_of(fs.apply_chown_ino(old, 1, 1)), stale());
    assert_eq!(errno_of(fs.apply_utimens_ino(old, 1, 0, 1, 0)), stale());
    assert_eq!(errno_of(fs.apply_link_at(old, ROOT, b"again")), stale());
    assert!(
        before == dev.snapshot(),
        "a stale handle changed the volume"
    );

    // The new generation, or none, reaches the new file.
    assert_eq!(
        fs.stat_ino(InodeRef::new(g, g_gen)).unwrap().generation,
        g_gen
    );
    assert_eq!(fs.stat_ino(InodeRef::any(g)).unwrap().generation, g_gen);
}

#[test]
fn a_stale_directory_handle_refuses_every_namespace_operation() {
    let (dev, fs) = mount(formatted(FsFlavor::Ext4));
    let d = fs.apply_mkdir_at(ROOT, b"d", 0o755).unwrap();
    let d_ref = InodeRef::new(d, fs.stat_ino(d).unwrap().generation);
    let f = fs.apply_create_at(ROOT, b"f", 0o644).unwrap();
    fs.apply_rmdir_at(ROOT, b"d").unwrap();
    let e = fs.apply_mkdir_at(ROOT, b"e", 0o755).unwrap();
    assert_eq!(
        e, d,
        "the allocator must reuse the freed slot for this test to mean anything"
    );

    let before = dev.snapshot();
    assert_eq!(errno_of(fs.lookup_at(d_ref, b".")), stale());
    assert_eq!(errno_of(fs.read_dir_ino(d_ref)), stale());
    assert_eq!(errno_of(fs.apply_create_at(d_ref, b"x", 0o644)), stale());
    assert_eq!(errno_of(fs.apply_mkdir_at(d_ref, b"x", 0o755)), stale());
    assert_eq!(
        errno_of(fs.apply_mknod_at(d_ref, b"x", 0o010644, 0, 0)),
        stale()
    );
    assert_eq!(errno_of(fs.apply_symlink_at(d_ref, b"x", b"t")), stale());
    assert_eq!(errno_of(fs.apply_link_at(f, d_ref, b"x")), stale());
    assert_eq!(errno_of(fs.apply_unlink_at(d_ref, b"x")), stale());
    assert_eq!(errno_of(fs.apply_rmdir_at(d_ref, b"x")), stale());
    assert_eq!(
        errno_of(fs.apply_rename_at(ROOT, b"f", d_ref, b"x", false)),
        stale()
    );
    assert_eq!(
        errno_of(fs.apply_rename_at(d_ref, b"x", ROOT, b"y", false)),
        stale()
    );
    assert!(
        before == dev.snapshot(),
        "a stale handle changed the volume"
    );
}

#[test]
fn an_inode_number_that_names_no_file_is_stale() {
    let (_dev, fs) = mount(formatted(FsFlavor::Ext4));
    let total = fs.sb.inodes_count;
    for ino in [0, 1, 7, 8, total + 1, u32::MAX] {
        assert_eq!(errno_of(fs.stat_ino(ino)), stale(), "inode {ino}");
    }
    // Never allocated.
    assert_eq!(errno_of(fs.stat_ino(total)), stale());
    assert_eq!(errno_of(fs.apply_create_at(total, b"x", 0o644)), stale());
    // The root is always reachable.
    assert!(fs.stat_ino(ROOT).unwrap().is_dir());
}

#[test]
fn names_are_bytes_and_are_validated() {
    let (_dev, fs) = mount(formatted(FsFlavor::Ext4));
    let einval = Error::InvalidArgument("").to_errno();

    // Not UTF-8, and it survives byte for byte.
    let odd: &[u8] = b"caf\xe9-\xff\xfe";
    let ino = fs.apply_create_at(ROOT, odd, 0o644).unwrap();
    assert_eq!(fs.lookup_at(ROOT, odd).unwrap(), ino);
    assert!(fs
        .read_dir_ino(ROOT)
        .unwrap()
        .iter()
        .any(|e| e.name == odd && e.inode == ino));

    for bad in [&b""[..], b"a/b", b"a\0b"] {
        assert_eq!(
            errno_of(fs.apply_create_at(ROOT, bad, 0o644)),
            einval,
            "{bad:?}"
        );
        assert_eq!(errno_of(fs.lookup_at(ROOT, bad)), einval, "{bad:?}");
        assert_eq!(errno_of(fs.apply_unlink_at(ROOT, bad)), einval, "{bad:?}");
    }
    assert_eq!(
        errno_of(fs.apply_create_at(ROOT, &[b'n'; 256], 0o644)),
        Error::NameTooLong.to_errno()
    );
    assert!(fs.apply_create_at(ROOT, &[b'n'; 255], 0o644).is_ok());
    assert_eq!(errno_of(fs.apply_symlink_at(ROOT, b"s", b"a\0b")), einval);
}

#[test]
fn symlink_targets_leave_room_for_the_terminating_byte() {
    // The kernel comparison is in tests/symlink_target_kernel.rs (#536).
    for block_size in [1024u32, 2048, 4096] {
        let dev = MemDev::with(vec![0u8; SIZE as usize]);
        format_filesystem_with_flavor(
            dev.as_ref(),
            None,
            Some([0x36; 16]),
            SIZE,
            block_size,
            FsFlavor::Ext4,
        )
        .unwrap();
        let fs = Filesystem::mount(dev.clone()).unwrap();
        let limit = block_size as usize - 1;
        for len in [limit - 1, limit, limit + 1, limit + 2] {
            let target = "x".repeat(len);
            for by_inode in [false, true] {
                let name = format!("link-{len}-{by_inode}");
                let before = dev.snapshot();
                let result = if by_inode {
                    fs.apply_symlink_at(ROOT, name.as_bytes(), target.as_bytes())
                } else {
                    fs.apply_symlink(&target, &format!("/{name}"))
                };
                if len <= limit {
                    let ino = result.unwrap();
                    assert_eq!(fs.read_link(ino).unwrap(), target.as_bytes());
                } else {
                    assert!(
                        matches!(result, Err(Error::NameTooLong)),
                        "block size {block_size}, length {len}, by inode {by_inode}: {result:?}"
                    );
                    assert!(
                        before == dev.snapshot(),
                        "a refused target changed the image"
                    );
                    assert!(matches!(
                        fs.lookup_at(ROOT, name.as_bytes()),
                        Err(Error::NotFound)
                    ));
                }
            }
        }
    }
}

#[test]
fn dot_and_dotdot_are_refused_where_they_would_corrupt() {
    let (dev, fs) = mount(formatted(FsFlavor::Ext4));
    let d = fs.apply_mkdir_at(ROOT, b"d", 0o755).unwrap();
    let before = dev.snapshot();
    let einval = Error::InvalidArgument("").to_errno();

    // Removing `.` would free the directory under its own parent's entry.
    assert_eq!(errno_of(fs.apply_rmdir_at(d, b".")), einval);
    assert_eq!(
        errno_of(fs.apply_rmdir_at(d, b"..")),
        Error::DirectoryNotEmpty.to_errno()
    );
    assert_eq!(
        errno_of(fs.apply_rename_at(d, b".", ROOT, b"x", false)),
        einval
    );
    assert_eq!(
        errno_of(fs.apply_rename_at(ROOT, b"d", d, b"..", true)),
        einval
    );
    // Existing names, so these fail as any existing name would.
    assert_eq!(
        errno_of(fs.apply_create_at(d, b".", 0o644)),
        Error::AlreadyExists.to_errno()
    );
    assert_eq!(
        errno_of(fs.apply_mkdir_at(d, b"..", 0o755)),
        Error::AlreadyExists.to_errno()
    );
    assert_eq!(
        errno_of(fs.apply_unlink_at(d, b".")),
        Error::IsADirectory.to_errno()
    );
    assert!(
        before == dev.snapshot(),
        "a refused operation changed the volume"
    );

    // And the path twins agree.
    assert_eq!(errno_of(fs.apply_rmdir("/d/.")), einval);
    assert_eq!(errno_of(fs.apply_rename("/d/.", "/x", false)), einval);
    assert!(
        before == dev.snapshot(),
        "a refused operation changed the volume"
    );
}

#[test]
fn a_directory_cannot_be_moved_under_itself_by_inode() {
    let (_dev, fs) = mount(formatted(FsFlavor::Ext4));
    let a = fs.apply_mkdir_at(ROOT, b"a", 0o755).unwrap();
    let b = fs.apply_mkdir_at(a, b"b", 0o755).unwrap();
    let c = fs.apply_mkdir_at(b, b"c", 0o755).unwrap();
    let einval = Error::InvalidArgument("").to_errno();
    assert_eq!(
        errno_of(fs.apply_rename_at(ROOT, b"a", c, b"x", false)),
        einval
    );
    assert_eq!(
        errno_of(fs.apply_rename_at(ROOT, b"a", a, b"x", false)),
        einval
    );
    assert_eq!(
        errno_of(fs.apply_rename_at(a, b"b", c, b"x", false)),
        einval
    );
    // A sibling subtree is fine.
    let z = fs.apply_mkdir_at(ROOT, b"z", 0o755).unwrap();
    fs.apply_rename_at(ROOT, b"a", z, b"a", false).unwrap();
    assert_eq!(walk(&fs, "/z/a/b/c").unwrap(), c);
}

#[test]
fn a_hard_link_and_a_directory_rename_do_not_disturb_an_inode_handle() {
    let (_dev, fs) = mount(formatted(FsFlavor::Ext4));
    let a = fs.apply_mkdir_at(ROOT, b"a", 0o755).unwrap();
    let b = fs.apply_mkdir_at(ROOT, b"b", 0o755).unwrap();
    let f = fs.apply_create_at(a, b"f", 0o644).unwrap();
    let handle = InodeRef::new(f, fs.stat_ino(f).unwrap().generation);
    fs.apply_link_at(handle, b, b"g").unwrap();
    fs.apply_rename_at(ROOT, b"a", ROOT, b"c", false).unwrap();

    fs.apply_pwrite_ino(handle, 0, b"through the handle")
        .unwrap();
    let mut out = [0u8; 18];
    for path in ["/c/f", "/b/g"] {
        let ino = walk(&fs, path).unwrap();
        assert_eq!(ino, f, "{path}");
        fs.read_ino(ino, 0, &mut out).unwrap();
        assert_eq!(&out, b"through the handle", "{path}");
    }
    fs.apply_unlink_at(b, b"g").unwrap();
    assert_eq!(fs.stat_ino(handle).unwrap().links_count, 1);
    fs.apply_unlink_at(a, b"f").unwrap();
    assert_eq!(errno_of(fs.stat_ino(handle)), stale());
}

#[test]
fn truncate_by_inode_refuses_what_truncate_by_path_refuses() {
    let (_dev, fs) = mount(formatted(FsFlavor::Ext4));
    let d = fs.apply_mkdir_at(ROOT, b"d", 0o755).unwrap();
    let s = fs.apply_symlink_at(ROOT, b"s", b"d").unwrap();
    let f = fs.apply_create_at(ROOT, b"f", 0o644).unwrap();
    assert_eq!(
        errno_of(fs.apply_truncate_ino(d, 0)),
        Error::IsADirectory.to_errno()
    );
    assert_eq!(
        errno_of(fs.apply_truncate_ino(s, 0)),
        Error::InvalidArgument("").to_errno()
    );
    fs.apply_pwrite_ino(f, 0, &[7u8; 5000]).unwrap();
    fs.apply_truncate_ino(f, 100).unwrap();
    assert_eq!(fs.stat_ino(f).unwrap().size, 100);
    fs.apply_truncate_ino(f, 9000).unwrap();
    assert_eq!(fs.stat_ino(f).unwrap().size, 9000);
}

/// The directory a path-addressed operation edits is the one the kernel
/// edits: a symlink naming the parent's last component is followed too,
/// so `unlink dirlink/x` removes `full/x` and `mkdir chain/sub` files
/// `sub` in `full`. Only the operation's own final name is not followed
/// (#564).
#[test]
fn a_symlink_naming_the_parent_is_followed_by_every_path_operation() {
    let (_dev, fs) = mount(formatted(FsFlavor::Ext4));
    fs.apply_mkdir("/full", 0o755).unwrap();
    fs.apply_create("/full/x", 0o644).unwrap();
    fs.apply_create("/full/y", 0o644).unwrap();
    fs.apply_symlink("full", "/dirlink").unwrap();
    fs.apply_symlink("/dirlink", "/chain").unwrap();
    fs.apply_symlink("y", "/full/ylink").unwrap();
    fs.apply_symlink("self", "/self").unwrap();

    fs.apply_unlink("/dirlink/x").unwrap();
    assert_eq!(errno_of(walk(&fs, "/full/x")), Error::NotFound.to_errno());
    fs.apply_mkdir("/chain/sub", 0o755).unwrap();
    assert!(fs
        .stat_ino(walk(&fs, "/full/sub").unwrap())
        .unwrap()
        .is_dir());
    fs.apply_rmdir("/chain/sub").unwrap();
    assert_eq!(errno_of(walk(&fs, "/full/sub")), Error::NotFound.to_errno());

    // The final name is the link itself, removed rather than followed.
    fs.apply_unlink("/chain/ylink").unwrap();
    assert_eq!(
        errno_of(walk(&fs, "/full/ylink")),
        Error::NotFound.to_errno()
    );
    assert!(walk(&fs, "/full/y").is_ok());

    assert_eq!(
        errno_of(fs.apply_unlink("/self/x")),
        Error::SymlinkLoop.to_errno()
    );
}
