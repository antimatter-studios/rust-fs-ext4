//! The driver writes; independent tools read it back.
//!
//! Every other write test in this crate checks the driver's work with the
//! driver's own reader, which shares every misreading of the on-disk
//! format the writer has. `e2fsck -fn` fixes half of that: it proves the
//! METADATA is consistent. It cannot see file CONTENT — a data block
//! with the wrong bytes in it is, as far as e2fsck is concerned, a
//! perfectly good block (`corrupted_data_is_caught_by_debugfs_not_by_e2fsck`
//! below proves exactly that). So each case here asks two independent
//! questions of e2fsprogs:
//!
//! - **consistency**: `e2fsck -fn` exits 0;
//! - **content and metadata**: `debugfs` dumps the file and it is byte
//!   identical to what was written (`dump` + compare), and `stat`, `ex`,
//!   `icheck` and `ncheck` agree on its size, type, mode, extents and
//!   the ownership of its blocks; `logdump` reads the journal the driver
//!   left behind.
//!
//! And the reverse direction: files placed by `mke2fs -d` (e2fsprogs'
//! own writer) are read through the driver and compared byte for byte.
//!
//! Entry points under test: `FileDevice::open_rw` → `Filesystem::mount`
//! → `apply_mkdir` / `apply_create` / `apply_pwrite`, and the C ABI
//! (`fs_ext4_mount_rw` / `fs_ext4_create` / `fs_ext4_write_file` /
//! `fs_ext4_pwrite` / `fs_ext4_umount`).
//!
//! The images are built here with `mke2fs` — no kernel mount, no fixture.
//! Every e2fsprogs call, this one included, runs in the harness VM
//! (`fs_ext4_test_support::oracle`), which is where those tools live and
//! the only place they are ever run.

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::capi::*;
use fs_ext4::fs::Filesystem;
use std::ffi::{CStr, CString};
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

const BLOCK: u64 = 4096;
const MIB: usize = 1024 * 1024;
const UUID: &str = "0e4a0c1e-0000-4000-8000-00000000d1ff";
const HASH_SEED: &str = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";

/// Where the big file's first byte lands: past three whole blocks of hole
/// and then 1234 bytes into the fourth, so neither the write nor the hole
/// in front of it lines up with a block.
const BIG_OFFSET: u64 = 3 * BLOCK + 1234;
/// Where the big write is split in two. Not a block multiple either, so
/// the second chunk starts in the middle of a block the first one ended in.
const BIG_SPLIT: usize = 400_001;

// ---------------------------------------------------------------------------
// Scratch space and deterministic content
// ---------------------------------------------------------------------------

/// A per-test scratch directory, removed on success and kept on failure
/// (the path is in the panic message) so the image can be inspected.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = fs_ext4_test_support::temp_dir()
            .join(format!("fs_ext4_oracle_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch(dir)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Deterministic, incompressible-looking bytes (xorshift64*), so a
/// misplaced block cannot compare equal by being the same as its
/// neighbour.
fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        for b in v.to_le_bytes() {
            if out.len() < len {
                out.push(b);
            }
        }
    }
    out
}

/// The first offset at which two byte strings differ, or `None` when they
/// are identical (length included). This is THE comparison every
/// positive case relies on, which is why the negative case runs it too.
fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    if let Some(i) = a.iter().zip(b).position(|(x, y)| x != y) {
        return Some(i);
    }
    (a.len() != b.len()).then(|| a.len().min(b.len()))
}

fn assert_identical(what: &str, expected: &[u8], actual: &[u8]) {
    if let Some(at) = first_difference(expected, actual) {
        panic!(
            "{what}: content differs at byte {at} (expected {} bytes, got {}; \
             expected byte {:?}, got {:?})",
            expected.len(),
            actual.len(),
            expected.get(at),
            actual.get(at),
        );
    }
}

// ---------------------------------------------------------------------------
// The oracle tools
// ---------------------------------------------------------------------------

struct Output {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(tool: &str, args: &[&str]) -> Output {
    let out = fs_ext4_test_support::oracle(tool)
        .args(args)
        // A fixed clock for mke2fs, so the images are the same every run.
        .env("E2FSPROGS_FAKE_TIME", "1700000000")
        .output();
    let o = Output {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    };
    eprintln!("[oracle] {tool} {} -> exit {}", args.join(" "), o.code);
    o
}

/// `debugfs -R <request> <image>`, read-only. debugfs reports a failed
/// request on stderr and still exits 0, so a request that printed an
/// error line is a failure here.
fn debugfs(image: &Path, request: &str) -> String {
    let out = fs_ext4_test_support::oracle("debugfs")
        .args(["-R", request, image.to_str().unwrap()])
        .judged()
        .clean(&format!("debugfs -R '{request}' {}", image.display()));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `e2fsck -fn`: forced full check, answering no to every repair, so it
/// reports without touching the image. Exit 0 is not the verdict (#280):
/// `-n` exits 0 having answered "no" to a wrong count, so the report is
/// read.
fn e2fsck_clean(image: &Path) {
    let out = fs_ext4_test_support::oracle("e2fsck")
        .args(["-fn", image.to_str().unwrap()])
        .judged()
        .clean(&image.display().to_string());
    eprintln!(
        "[oracle] e2fsck -fn clean: {}",
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .last()
            .unwrap_or("")
    );
}

/// Dump `path` out of the image with debugfs and return its bytes.
fn debugfs_dump(image: &Path, path: &str, scratch: &Scratch) -> Vec<u8> {
    let out = scratch.path("dumped.bin");
    let _ = std::fs::remove_file(&out);
    debugfs(image, &format!("dump {path} {}", out.display()));
    std::fs::read(&out).unwrap_or_else(|e| panic!("debugfs dump {path} wrote nothing: {e}"))
}

/// `mke2fs -d <seed>`: a 64 MiB ext4 image with a journal and
/// metadata_csum, populated by e2fsprogs itself, UUID and hash seed
/// pinned.
fn mke2fs_image(scratch: &Scratch, seed: Option<&Path>) -> PathBuf {
    let image = scratch.path("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(64 * MIB as u64))
        .expect("size image");
    let mut args = vec![
        "-q",
        "-F",
        "-t",
        "ext4",
        "-b",
        "4096",
        "-O",
        "has_journal,extent,metadata_csum,^orphan_file,^metadata_csum_seed",
        "-U",
        UUID,
    ];
    let ext = format!("hash_seed={HASH_SEED}");
    args.extend(["-E", &ext]);
    let seed_str;
    if let Some(s) = seed {
        seed_str = s.to_str().unwrap().to_owned();
        args.extend(["-d", &seed_str]);
    }
    let image_str = image.to_str().unwrap().to_owned();
    args.push(&image_str);
    let o = run("mke2fs", &args);
    assert_eq!(o.code, 0, "mke2fs failed: {}{}", o.stdout, o.stderr);
    image
}

/// A `Key: value` field from debugfs `stat`.
fn stat_field<'a>(stat: &'a str, key: &str) -> &'a str {
    let at = stat
        .find(&format!("{key}:"))
        .unwrap_or_else(|| panic!("debugfs stat has no {key}:\n{stat}"));
    stat[at + key.len() + 1..]
        .split_whitespace()
        .next()
        .unwrap_or_else(|| panic!("debugfs stat {key}: has no value"))
}

/// Leaf extents from debugfs `ex`, as (logical start, logical end,
/// physical start). A line reads
///
/// ```text
///  0/ 0   1/  1     0 -   732  2065 -  2797    733
/// ```
///
/// level/max, entry/count, logical range, physical range, length; a leaf
/// is the line whose level equals the tree's max depth.
fn extents(ex: &str) -> Vec<(u64, u64, u64)> {
    ex.lines()
        .filter_map(|line| {
            let n: Vec<u64> = line
                .replace('/', " ")
                .split_whitespace()
                .map_while(|t| {
                    if t == "-" {
                        Some(None)
                    } else {
                        t.parse().ok().map(Some)
                    }
                })
                .flatten()
                .collect();
            (n.len() >= 9 && n[0] == n[1]).then(|| (n[4], n[5], n[6]))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Driver-side helpers
// ---------------------------------------------------------------------------

fn mount_rw(image: &Path) -> Filesystem {
    let dev = FileDevice::open_rw(image.to_str().unwrap()).expect("open_rw");
    Filesystem::mount(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount rw")
}

/// Read a whole file through the driver (lookup + file_io::read).
fn driver_read(fs: &Filesystem, path: &str) -> Vec<u8> {
    let mut read_inode = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    let ino = fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut read_inode, path)
        .unwrap_or_else(|e| panic!("driver lookup {path}: {e}"));
    let (inode, _) = fs.read_inode_verified(ino).expect("read inode");
    let mut buf = vec![0u8; inode.size as usize];
    let n = fs_ext4::file_io::read(fs, &inode, 0, inode.size, &mut buf)
        .unwrap_or_else(|e| panic!("driver read {path}: {e}"));
    buf.truncate(n as usize);
    buf
}

/// The driver's writes the debugfs cases check: a directory, a small
/// file, and a 1 MiB file written in two chunks at a non-block-aligned
/// offset. Returns the expected content of the big file.
fn driver_write_tree(image: &Path) -> Vec<u8> {
    let payload = pattern(1, MIB);
    let fs = mount_rw(image);
    fs.apply_mkdir("/written", 0o755).expect("apply_mkdir");
    fs.apply_create("/written/small.txt", 0o600)
        .expect("apply_create small");
    fs.apply_pwrite("/written/small.txt", 0, b"written by fs-ext4\n")
        .expect("apply_pwrite small");
    fs.apply_create("/written/big.bin", 0o644)
        .expect("apply_create big");
    let size = fs
        .apply_pwrite("/written/big.bin", BIG_OFFSET, &payload[..BIG_SPLIT])
        .expect("apply_pwrite chunk 1");
    assert_eq!(size, BIG_OFFSET + BIG_SPLIT as u64);
    let size = fs
        .apply_pwrite(
            "/written/big.bin",
            BIG_OFFSET + BIG_SPLIT as u64,
            &payload[BIG_SPLIT..],
        )
        .expect("apply_pwrite chunk 2");
    assert_eq!(size, BIG_OFFSET + MIB as u64);
    drop(fs);

    let mut expected = vec![0u8; BIG_OFFSET as usize];
    expected.extend_from_slice(&payload);
    expected
}

// ---------------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------------

/// Driver writes through the Rust API; debugfs reads every byte and every
/// piece of metadata back; e2fsck finds the filesystem consistent.
#[test]
fn driver_writes_are_read_back_identically_by_debugfs() {
    let scratch = Scratch::new("rust_api");
    let image = mke2fs_image(&scratch, None);
    let expected = driver_write_tree(&image);

    // Content: dump + compare.
    let dumped = debugfs_dump(&image, "/written/big.bin", &scratch);
    assert_identical("debugfs dump /written/big.bin", &expected, &dumped);
    eprintln!(
        "[oracle] debugfs dump /written/big.bin: {} bytes, identical to what the driver wrote",
        dumped.len()
    );
    let small = debugfs_dump(&image, "/written/small.txt", &scratch);
    assert_identical(
        "debugfs dump /written/small.txt",
        b"written by fs-ext4\n",
        &small,
    );

    // Metadata: stat.
    let stat = debugfs(&image, "stat /written/big.bin");
    let ino: u32 = stat_field(&stat, "Inode").parse().expect("inode number");
    assert_eq!(stat_field(&stat, "Type"), "regular", "{stat}");
    assert_eq!(stat_field(&stat, "Mode"), "0644", "{stat}");
    assert_eq!(
        stat_field(&stat, "Size").parse::<u64>().unwrap(),
        expected.len() as u64,
        "{stat}"
    );
    assert_eq!(stat_field(&stat, "Links"), "1", "{stat}");
    let dstat = debugfs(&image, "stat /written");
    assert_eq!(stat_field(&dstat, "Type"), "directory", "{dstat}");
    assert_eq!(stat_field(&dstat, "Mode"), "0755", "{dstat}");
    let sstat = debugfs(&image, "stat /written/small.txt");
    assert_eq!(stat_field(&sstat, "Mode"), "0600", "{sstat}");

    // Extents: every block holding data is mapped, the hole in front of
    // it is not, and nothing is mapped past the end of the file.
    let ex = debugfs(&image, "ex /written/big.bin");
    let leaves = extents(&ex);
    assert!(!leaves.is_empty(), "debugfs ex found no extents:\n{ex}");
    let first_data = BIG_OFFSET / BLOCK;
    let last_data = (expected.len() as u64 - 1) / BLOCK;
    let mut mapped: Vec<u64> = leaves.iter().flat_map(|&(l, e, _)| l..=e).collect();
    mapped.sort_unstable();
    let want: Vec<u64> = (first_data..=last_data).collect();
    assert_eq!(
        mapped, want,
        "extents do not map exactly logical blocks {first_data}..={last_data}:\n{ex}"
    );
    eprintln!(
        "[oracle] debugfs ex: {} leaf extent(s) map logical {first_data}..={last_data}",
        leaves.len()
    );

    // Block ownership: icheck says the first and last data blocks belong
    // to this inode, and ncheck names the inode by its path.
    let (l0, _, p0) = leaves[0];
    let &(ll, le, pl) = leaves.last().unwrap();
    let first_phys = p0 + (first_data - l0);
    let last_phys = pl + (le - ll);
    let icheck = debugfs(&image, &format!("icheck {first_phys} {last_phys}"));
    for phys in [first_phys, last_phys] {
        let line = icheck
            .lines()
            .find(|l| l.split_whitespace().next() == Some(&phys.to_string()))
            .unwrap_or_else(|| panic!("icheck has no line for block {phys}:\n{icheck}"));
        assert_eq!(
            line.split_whitespace().nth(1),
            Some(ino.to_string().as_str()),
            "icheck: block {phys} is not owned by inode {ino}:\n{icheck}"
        );
    }
    let ncheck = debugfs(&image, &format!("ncheck {ino}"));
    assert!(
        ncheck.lines().any(|l| {
            // debugfs prints a doubled leading slash for some paths.
            let t: Vec<String> = l.split_whitespace().map(|s| s.replace("//", "/")).collect();
            t == [ino.to_string(), "/written/big.bin".to_string()]
        }),
        "ncheck {ino} does not name /written/big.bin:\n{ncheck}"
    );

    // The journal the driver left: debugfs can walk it, and nothing is
    // waiting to be replayed.
    let logdump = debugfs(&image, "logdump");
    assert!(
        logdump.contains("Journal starts at block"),
        "debugfs logdump did not read the journal:\n{logdump}"
    );
    let features = debugfs(&image, "features");
    assert!(
        !features.contains("needs_recovery"),
        "the driver left the journal needing recovery: {features}"
    );

    // Consistency.
    e2fsck_clean(&image);
}

/// The other direction: e2fsprogs writes (`mke2fs -d`), the driver reads.
#[test]
fn mke2fs_placed_files_are_read_identically_by_the_driver() {
    let scratch = Scratch::new("reverse");
    let seed = scratch.path("seed");
    std::fs::create_dir_all(seed.join("dir/deeper")).unwrap();
    let big = pattern(2, MIB + 777);
    let mid = pattern(3, 3 * BLOCK as usize + 5);
    std::fs::write(seed.join("hello.txt"), b"hello from mke2fs\n").unwrap();
    std::fs::write(seed.join("dir/big.bin"), &big).unwrap();
    std::fs::write(seed.join("dir/deeper/mid.bin"), &mid).unwrap();
    std::fs::write(seed.join("empty"), b"").unwrap();

    let image = mke2fs_image(&scratch, Some(&seed));
    e2fsck_clean(&image);

    let dev = FileDevice::open(image.to_str().unwrap()).expect("open");
    let fs = Filesystem::mount(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount");
    assert_identical(
        "driver read /hello.txt",
        b"hello from mke2fs\n",
        &driver_read(&fs, "/hello.txt"),
    );
    assert_identical(
        "driver read /dir/big.bin",
        &big,
        &driver_read(&fs, "/dir/big.bin"),
    );
    assert_identical(
        "driver read /dir/deeper/mid.bin",
        &mid,
        &driver_read(&fs, "/dir/deeper/mid.bin"),
    );
    assert_identical("driver read /empty", b"", &driver_read(&fs, "/empty"));
    eprintln!(
        "[oracle] driver read of mke2fs -d files: {} + {} bytes identical",
        big.len(),
        mid.len()
    );

    // And debugfs agrees with its own writer, so the comparison above is
    // against the bytes that are really on disk.
    assert_identical(
        "debugfs dump /dir/big.bin",
        &big,
        &debugfs_dump(&image, "/dir/big.bin", &scratch),
    );
}

fn last_err() -> String {
    unsafe {
        let p = fs_ext4_last_error();
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

/// The same checks through the C ABI a real consumer (FSKit, WinFsp)
/// calls: mount_rw, create, write_file, pwrite, umount.
#[test]
fn c_abi_writes_are_read_back_identically_by_debugfs() {
    let scratch = Scratch::new("capi");
    let image = mke2fs_image(&scratch, None);
    let c_image = CString::new(image.to_str().unwrap()).unwrap();
    let path = CString::new("/capi.bin").unwrap();

    let head = pattern(4, 10_000);
    let tail = pattern(5, MIB);
    let tail_offset: u64 = 7_777; // inside the replaced body, not on a block
    unsafe {
        let fs = fs_ext4_mount_rw(c_image.as_ptr());
        assert!(!fs.is_null(), "fs_ext4_mount_rw: {}", last_err());
        let ino = fs_ext4_create(fs, path.as_ptr(), 0o640);
        assert!(ino > 0, "fs_ext4_create: {}", last_err());
        let n = fs_ext4_write_file(
            fs,
            path.as_ptr(),
            head.as_ptr() as *const c_void,
            head.len() as u64,
        );
        assert_eq!(n, head.len() as i64, "fs_ext4_write_file: {}", last_err());
        let n = fs_ext4_pwrite(
            fs,
            path.as_ptr(),
            tail.as_ptr() as *const c_void,
            tail.len() as u64,
            tail_offset,
        );
        assert_eq!(
            n,
            tail_offset as i64 + tail.len() as i64,
            "fs_ext4_pwrite: {}",
            last_err()
        );
        fs_ext4_umount(fs);
    }

    let mut expected = head[..tail_offset as usize].to_vec();
    expected.extend_from_slice(&tail);
    let dumped = debugfs_dump(&image, "/capi.bin", &scratch);
    assert_identical("debugfs dump /capi.bin", &expected, &dumped);
    eprintln!(
        "[oracle] debugfs dump /capi.bin: {} bytes identical (C ABI)",
        dumped.len()
    );
    let stat = debugfs(&image, "stat /capi.bin");
    assert_eq!(stat_field(&stat, "Mode"), "0640", "{stat}");
    assert_eq!(
        stat_field(&stat, "Size").parse::<u64>().unwrap(),
        expected.len() as u64
    );
    e2fsck_clean(&image);
}

/// Why debugfs is in the oracle at all: flip ONE byte of file data on
/// disk after a correct driver write. e2fsck — which checks metadata —
/// still passes; the debugfs dump + compare the other cases rely on
/// catches it, at the byte that was flipped.
#[test]
fn corrupted_data_is_caught_by_debugfs_not_by_e2fsck() {
    let scratch = Scratch::new("negative");
    let image = mke2fs_image(&scratch, None);
    let expected = driver_write_tree(&image);

    // Pick a byte in the middle of the second chunk and find the block
    // it lives in with debugfs itself.
    let target: u64 = BIG_OFFSET + BIG_SPLIT as u64 + 12_345;
    let lblk = target / BLOCK;
    let bmap = debugfs(&image, &format!("bmap /written/big.bin {lblk}"));
    let pblk: u64 = bmap
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("bmap: {bmap}"));
    assert!(pblk > 0, "logical block {lblk} is not mapped");
    let disk_offset = pblk * BLOCK + target % BLOCK;
    {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&image)
            .unwrap();
        let mut b = [0u8];
        f.seek(SeekFrom::Start(disk_offset)).unwrap();
        f.read_exact(&mut b).unwrap();
        f.seek(SeekFrom::Start(disk_offset)).unwrap();
        f.write_all(&[b[0] ^ 0xA5]).unwrap();
    }
    eprintln!("[oracle] corrupted file byte {target} (block {pblk}, disk offset {disk_offset})");

    // e2fsck is blind to it: the metadata is untouched.
    e2fsck_clean(&image);

    // The content oracle is not.
    let dumped = debugfs_dump(&image, "/written/big.bin", &scratch);
    assert_eq!(
        first_difference(&expected, &dumped),
        Some(target as usize),
        "debugfs dump + compare must report the flipped byte and only that one"
    );
    eprintln!(
        "[oracle] debugfs dump + compare caught the corruption at byte {target}; e2fsck did not"
    );
}
