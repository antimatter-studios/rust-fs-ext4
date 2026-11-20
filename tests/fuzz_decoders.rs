//! The stable-toolchain half of the fuzzing setup: replay the corpus,
//! then mutate it, and refuse if a decoder panics, hangs, or if the
//! suite quietly stopped doing any work.
//!
//! # Why there are two halves
//!
//! `fuzz/` holds `cargo-fuzz` targets. Those are the explorer: they run
//! for as long as you give them and find inputs nobody thought of. They
//! cannot be a required check, because how long they ran decides what
//! they found, and a fresh discovery would fail whichever unrelated
//! pull request happened to be open.
//!
//! This suite is the gate. Deterministic, on the stable toolchain, in
//! every pull request, reading the same `fuzz/corpus/` the explorer
//! does. Anything the explorer finds is committed there and replayed
//! here from then on.
//!
//! # Why the corpus is whole filesystems
//!
//! ext4 is the widest parser surface in the family: a superblock, group
//! descriptors, an inode table, extent trees, htree indexes and a jbd2
//! journal, each read from an offset the one before it supplied. Most
//! of that is unreachable from a byte slice, so the corpus holds four
//! filesystems `mke2fs` wrote and `image` mutates and mounts them.
//!
//! One per shape that changes a decoder rather than the layout: ext2,
//! which has neither extents nor a journal; ext4 at 1 KiB blocks, which
//! moves every offset; ext4 at 4 KiB; and ext4 with `64bit` and
//! `metadata_csum`, which widens the group descriptors and adds a
//! checksum in front of several walks.
//!
//! They are 4 to 8 MiB each, under the 10 MiB ceiling github-guard
//! enforces on a committed file, and nearly all zeros, so git stores
//! them in a fraction of that.
//!
//! # The journal is fuzzed at mount, not separately
//!
//! `walk` calls `replay_journal_if_dirty`. A journal is a structure the
//! format expects to be partially written -- that is its purpose -- so
//! it is parsed with a corruption tolerance the other structures do not
//! have, and it runs before anything about the filesystem has been
//! established. The `journal` target takes the superblock on its own as
//! well, but the interesting path is the one a mount takes.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// Where this suite looks for the corpus.
fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus")
}

// The in-memory device and the bounded walk, shared verbatim with the
// explorer. See fuzz/shared/walk.rs for why they are included rather
// than depended on.
include!("../fuzz/shared/walk.rs");

/// Distinct starting points for the mutation stream. Fixed, so a
/// failure reproduces from the message alone.
const SEEDS: u64 = 6;

/// Below this, the suite is not doing its job.
const CASE_FLOOR: usize = 6_000;

/// How long the run may go without finishing a single case before it is
/// called a hang. It bounds one case, not the whole run, so an emulated
/// guest that takes minutes in total is never mistaken for a hang.
/// `FS_EXT4_FUZZ_DEADLINE_SECS` overrides it for a slower environment.
const DEADLINE_SECS: u64 = 360;

/// Upper limit on the whole run, so a run that keeps making progress but
/// never finishes is still stopped. `FS_EXT4_FUZZ_TOTAL_SECS` overrides it.
const TOTAL_SECS: u64 = 3600;

fn budget(var: &str, default: u64) -> Duration {
    let secs = std::env::var(var)
        .ok()
        .map(|v| {
            v.parse::<u64>()
                .unwrap_or_else(|_| panic!("{var}={v:?} is not a number of seconds"))
        })
        .unwrap_or(default);
    Duration::from_secs(secs)
}

/// How often the watchdog looks at the case counter.
const POLL: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------- targets

struct Target {
    corpus: &'static str,
    name: &'static str,
    /// Mutated cases per (seed, starting point) pair.
    ///
    /// Per target rather than one constant, because a case costs what
    /// its seed costs to copy and to run. A 64 KiB region table is
    /// cheap; a 16 MB image opened and read is not, and giving both the
    /// same budget would mean either a slow gate or a shallow one.
    cases: usize,
    run: fn(&[u8]),
}

fn targets() -> Vec<Target> {
    vec![
        Target {
            corpus: "image",
            name: "image",
            cases: 24,
            run: walk,
        },
        Target {
            corpus: "superblock",
            name: "superblock",
            cases: 256,
            run: |b| {
                let _ = fs_ext4::Superblock::parse(b.to_vec());
            },
        },
        Target {
            corpus: "inode",
            name: "inode",
            cases: 256,
            run: |b| {
                if let Ok(inode) = fs_ext4::inode::Inode::parse(b) {
                    // `block` is sixty bytes that mean different things
                    // depending on the flags: an extent tree, twelve
                    // direct block numbers and three indirect ones, or
                    // inline file data.
                    let _ = fs_ext4::extent::ExtentHeader::parse(&inode.block);
                    let _ = fs_ext4::extent::Extent::parse(&inode.block);
                    let _ = fs_ext4::extent::ExtentIdx::parse(&inode.block);
                }
            },
        },
        Target {
            corpus: "dir_block",
            name: "dir_block",
            cases: 256,
            run: |b| {
                for has_file_type in [true, false] {
                    let _ = fs_ext4::dir::parse_block(b, has_file_type);
                }
                let _ = fs_ext4::dir::has_csum_tail(b);
                let _ = fs_ext4::htree::parse_root_info(b);
                let _ = fs_ext4::htree::parse_root_entries(b);
                let _ = fs_ext4::htree::parse_node_entries(b);
            },
        },
        Target {
            corpus: "journal",
            name: "journal",
            cases: 256,
            run: |b| {
                let _ = fs_ext4::jbd2::JournalSuperblock::parse(b);
            },
        },
    ]
}

// ---------------------------------------------------------------- corpus

fn seed_paths(corpus: &str) -> Vec<(String, PathBuf)> {
    let dir = corpus_root().join(corpus);
    let mut out: Vec<(String, PathBuf)> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading the corpus directory {}: {e}", dir.display()))
        .map(|entry| {
            let path = entry.expect("corpus directory entry").path();
            let name = path
                .file_name()
                .expect("seed file name")
                .to_string_lossy()
                .into_owned();
            (name, path)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn seeds(corpus: &str) -> Vec<(String, Vec<u8>)> {
    seed_paths(corpus)
        .into_iter()
        .map(|(name, path)| {
            let bytes = fs_ext4_test_support::read_small_file(&path)
                .unwrap_or_else(|e| panic!("reading the seed {}: {e}", path.display()));
            (name, bytes)
        })
        .collect()
}

// ---------------------------------------------------------------- mutation

/// xorshift64*. Small, deterministic, and not a dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
}

/// One mutation of a real structure or a real image, preserving length.
///
/// Length is preserved because a device answers a read past its end
/// with `ShortRead` before any of this code is reached -- a hostile
/// image controls what is in a block, not how many bytes the device
/// hands back.
///
/// The `header` bias exists because an image is mostly file data: a
/// uniformly random offset in a 48 KiB image lands in somebody's text
/// file nine times out of ten, where nothing parses it. Half the
/// mutations are aimed at the first two blocks, which is where the
/// superblock, the inode table and the directory blocks are.
fn mutate(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = seed.to_vec();
    if out.is_empty() {
        return out;
    }
    let metadata_end = out.len().min(8192);
    let region = if rng.next() & 1 == 0 {
        metadata_end
    } else {
        out.len()
    };

    match rng.below(5) {
        0 => {
            for _ in 0..=rng.below(8) {
                let at = rng.below(region);
                out[at] ^= 1u8 << rng.below(8);
            }
        }
        1 => {
            let at = rng.below(region);
            let len = 1 + rng.below(16.min(out.len() - at));
            let fill = if rng.next() & 1 == 0 { 0x00 } else { 0xff };
            out[at..at + len].fill(fill);
        }
        2 => {
            let width = [2usize, 4, 8][rng.below(3)];
            if out.len() >= width {
                let at = rng.below(region.saturating_sub(width) + 1) & !(width - 1);
                if at + width <= out.len() {
                    let value: u64 = match rng.below(4) {
                        0 => 0,
                        1 => 1,
                        2 => u64::MAX,
                        _ => rng.next(),
                    };
                    // Little-endian: every multi-byte field in partition table is.
                    out[at..at + width].copy_from_slice(&value.to_le_bytes()[..width]);
                }
            }
        }
        3 => {
            if out.len() >= 8 {
                let a = rng.below(region / 4) * 4;
                let b = rng.below(region / 4) * 4;
                if a + 4 <= out.len() && b + 4 <= out.len() {
                    for i in 0..4 {
                        out.swap(a + i, b + i);
                    }
                }
            }
        }
        _ => {
            if out.len() >= 4 {
                let at = rng.below(region / 4) * 4;
                if at + 4 <= out.len() {
                    let word = u32::from_le_bytes(out[at..at + 4].try_into().expect("4 bytes"));
                    let delta = [1i64, -1, 2, -2, 255, -255][rng.below(6)];
                    let changed = (i64::from(word).wrapping_add(delta)) as u32;
                    out[at..at + 4].copy_from_slice(&changed.to_le_bytes());
                }
            }
        }
    }
    out
}

fn read_at(file: &mut File, offset: u64, buffer: &mut [u8]) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buffer)
}

fn write_at(file: &mut File, offset: u64, buffer: &[u8]) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(buffer)
}

fn mutate_image(seed: &Path, destination: &Path, rng: &mut Rng) -> std::io::Result<()> {
    fs_ext4_test_support::copy_file_sparse(seed, destination)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(destination)?;
    let length = file.metadata()?.len() as usize;
    if length == 0 {
        return Ok(());
    }
    let metadata_end = length.min(8192);
    let region = if rng.next() & 1 == 0 {
        metadata_end
    } else {
        length
    };

    match rng.below(5) {
        0 => {
            for _ in 0..=rng.below(8) {
                let at = rng.below(region);
                let mut byte = [0];
                read_at(&mut file, at as u64, &mut byte)?;
                byte[0] ^= 1u8 << rng.below(8);
                write_at(&mut file, at as u64, &byte)?;
            }
        }
        1 => {
            let at = rng.below(region);
            let count = 1 + rng.below(16.min(length - at));
            let fill = if rng.next() & 1 == 0 { 0x00 } else { 0xff };
            write_at(&mut file, at as u64, &vec![fill; count])?;
        }
        2 => {
            let width = [2usize, 4, 8][rng.below(3)];
            let at = rng.below(region.saturating_sub(width) + 1) & !(width - 1);
            if at + width <= length {
                let value: u64 = match rng.below(4) {
                    0 => 0,
                    1 => 1,
                    2 => u64::MAX,
                    _ => rng.next(),
                };
                write_at(&mut file, at as u64, &value.to_le_bytes()[..width])?;
            }
        }
        3 => {
            if length >= 8 {
                let a = rng.below(region / 4) * 4;
                let b = rng.below(region / 4) * 4;
                if a + 4 <= length && b + 4 <= length {
                    let mut left = [0; 4];
                    let mut right = [0; 4];
                    read_at(&mut file, a as u64, &mut left)?;
                    read_at(&mut file, b as u64, &mut right)?;
                    write_at(&mut file, a as u64, &right)?;
                    write_at(&mut file, b as u64, &left)?;
                }
            }
        }
        _ => {
            if length >= 4 {
                let at = rng.below(region / 4) * 4;
                if at + 4 <= length {
                    let mut bytes = [0; 4];
                    read_at(&mut file, at as u64, &mut bytes)?;
                    let word = u32::from_le_bytes(bytes);
                    let delta = [1i64, -1, 2, -2, 255, -255][rng.below(6)];
                    let changed = (i64::from(word).wrapping_add(delta)) as u32;
                    write_at(&mut file, at as u64, &changed.to_le_bytes())?;
                }
            }
        }
    }
    Ok(())
}

fn walk_image_file(path: &Path) {
    let device: Arc<dyn fs_ext4::block_io::BlockDevice> = Arc::new(
        fs_ext4::block_io::FileDevice::open(path.to_str().expect("UTF-8 image path"))
            .expect("open mutated image"),
    );
    walk_device(device);
}

/// The case in flight, readable even if the lock was poisoned by the
/// panic we are trying to describe.
fn describe(current: &Arc<Mutex<String>>) -> String {
    match current.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

// ---------------------------------------------------------------- tests

#[test]
fn every_target_has_a_corpus() {
    for target in targets() {
        assert!(
            !seed_paths(target.corpus).is_empty(),
            "the target {} reads fuzz/corpus/{}, which holds no seeds -- a target with an \
             empty corpus runs no cases and would pass in silence. Rebuild it with \
             scripts/make-fuzz-corpus.sh",
            target.name,
            target.corpus,
        );
    }
}

/// The corpus is an oracle, not just fuel: every committed filesystem is
/// one `mke2fs` wrote, so this crate must mount it and find what the
/// tool put there.
///
/// A seed that stopped mounting would otherwise go on being mutated and
/// go on not failing, because a mutation of an unmountable filesystem
/// is also unmountable -- the corpus would still be there, testing
/// nothing.
#[test]
fn every_committed_filesystem_mounts_and_reads_its_root() {
    let images = seed_paths("image");
    assert_eq!(
        images.len(),
        4,
        "the image corpus holds {} filesystems, not the four the script builds",
        images.len()
    );

    for (name, path) in images {
        let dev: std::sync::Arc<dyn fs_ext4::block_io::BlockDevice> = std::sync::Arc::new(
            fs_ext4::block_io::FileDevice::open(path.to_str().unwrap()).unwrap(),
        );
        let fs = fs_ext4::Filesystem::mount(dev)
            .unwrap_or_else(|e| panic!("{name}: a filesystem mke2fs wrote would not mount: {e}"));

        let (root, _raw) = fs
            .read_inode_verified(2)
            .unwrap_or_else(|e| panic!("{name}: the root inode would not read: {e}"));
        assert!(
            root.mode & 0xF000 == 0x4000,
            "{name}: inode 2 is not a directory, so the inode table is not where we think"
        );

        // The tree the script builds has more than 600 entries in its
        // root, which is what pushes ext4 into an htree index. If this
        // ever reads as a handful, the corpus stopped exercising the
        // indexed directory path and nothing else would say so.
        assert!(
            root.size > 4096,
            "{name}: the root directory is {} bytes, too small to be the tree the script \
             builds -- the indexed-directory path is not being seeded",
            root.size
        );
    }
}

#[test]
fn deterministic_mutations_of_real_structures_are_survived() {
    let cases = Arc::new(AtomicUsize::new(0));
    let current = Arc::new(Mutex::new(String::from("(not started)")));
    let (done_tx, done_rx) = mpsc::channel();

    let hook_current = Arc::clone(&current);
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("\nfuzz gate: panicked at {}", describe(&hook_current));
        previous_hook(info);
    }));

    let worker_cases = Arc::clone(&cases);
    let worker_current = Arc::clone(&current);
    let worker = std::thread::spawn(move || {
        for target in targets() {
            if target.corpus == "image" {
                for (seed_name, seed_path) in seed_paths(target.corpus) {
                    for start in 0..SEEDS {
                        let mut rng = Rng::new(start);
                        for case in 0..target.cases {
                            *worker_current.lock().expect("progress lock") = format!(
                                "{} / {seed_name} / seed {start} / case {case}",
                                target.name
                            );
                            let destination = fs_ext4_test_support::temp_dir().join(format!(
                                "fuzz-image-{}-{start}-{case}.img",
                                std::process::id()
                            ));
                            mutate_image(&seed_path, &destination, &mut rng)
                                .unwrap_or_else(|e| panic!("mutate {}: {e}", seed_path.display()));
                            walk_image_file(&destination);
                            fs::remove_file(&destination).expect("remove mutated image");
                            worker_cases.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                continue;
            }
            for (seed_name, bytes) in seeds(target.corpus) {
                for start in 0..SEEDS {
                    let mut rng = Rng::new(start);
                    for case in 0..target.cases {
                        *worker_current.lock().expect("progress lock") =
                            format!("{} / {seed_name} / seed {start} / case {case}", target.name);
                        let mutated = mutate(&bytes, &mut rng);
                        (target.run)(&mutated);
                        worker_cases.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        let _ = done_tx.send(());
    });

    // A timeout means the worker is still running; it is a hang only when
    // no case has finished for `DEADLINE`. A disconnect means it
    // panicked, and the panic is what is worth reporting.
    let deadline = budget("FS_EXT4_FUZZ_DEADLINE_SECS", DEADLINE_SECS);
    let total = budget("FS_EXT4_FUZZ_TOTAL_SECS", TOTAL_SECS);
    let started = std::time::Instant::now();
    let mut last_count = 0;
    let mut last_progress = std::time::Instant::now();
    loop {
        match done_rx.recv_timeout(POLL) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let count = cases.load(Ordering::Relaxed);
                if count != last_count {
                    last_count = count;
                    last_progress = std::time::Instant::now();
                }
                if last_progress.elapsed() >= deadline || started.elapsed() >= total {
                    // Written to the process's stderr rather than through
                    // `eprintln!`, which the harness captures into a buffer it
                    // only prints when a test finishes -- and exiting here means
                    // it never finishes.
                    let _ = writeln!(
                        std::io::stderr(),
                        "\nhung: no progress for {:?} (run limit {:?}) at {}\n\
                         A decoder did not return. A directory record of length zero, or an \
                         extent tree that points back at itself, looks exactly like this.",
                        deadline,
                        total,
                        describe(&current),
                    );
                    let _ = std::io::stderr().flush();
                    std::process::exit(1);
                }
            }
        }
    }

    let outcome = worker.join();
    let _ = std::panic::take_hook();
    if outcome.is_err() {
        panic!("a decoder panicked at {}", describe(&current));
    }

    let total = cases.load(Ordering::Relaxed);
    assert!(
        total >= CASE_FLOOR,
        "only {total} mutated cases ran, below the floor of {CASE_FLOOR} -- the target \
         list or the corpus has collapsed, and a suite that runs nothing passes quickly",
    );
    eprintln!("{total} mutated cases");
}

#[test]
fn the_gate_covers_every_explorer_target() {
    // The two tiers drift apart the moment somebody adds a cargo-fuzz
    // target and forgets that nothing gates it on the stable toolchain.
    let manifest =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fuzz/Cargo.toml"))
            .expect("reading fuzz/Cargo.toml");

    let explorer: Vec<String> = manifest
        .lines()
        .filter_map(|line| line.strip_prefix("name = \""))
        .filter_map(|rest| rest.strip_suffix('"'))
        .map(str::to_owned)
        .skip(1) // the package name is the first `name =` in the file
        .collect();

    assert!(
        !explorer.is_empty(),
        "fuzz/Cargo.toml declares no [[bin]] targets",
    );

    let gated: Vec<&str> = targets().iter().map(|t| t.name).collect();
    for name in &explorer {
        assert!(
            gated.contains(&name.as_str()),
            "fuzz/fuzz_targets/{name}.rs has no counterpart in this suite, so nothing \
             replays its corpus on the stable toolchain and anything it finds would only \
             stay fixed for as long as somebody keeps running the fuzzer by hand",
        );
    }
}

/// `EXT4_INDEX_FL`, the inode flag that sends a directory lookup through
/// the htree rather than a linear scan.
const EXT4_INDEX_FL: u32 = 0x1000;

/// The corpus seeds the htree decoder, and keeps a linear directory too.
///
/// `mke2fs -d` writes every directory as a linear list however many
/// entries it holds, so a corpus built by it alone never hands the
/// explorer a real `dx_root`: the htree decoder only ever sees what a
/// mutation of a linear block happens to look like. The script indexes
/// the ext4 filesystems after populating them; this is what says it did.
#[test]
fn the_corpus_seeds_an_indexed_directory_and_a_linear_one() {
    let root_flags = |bytes: &[u8]| u32::from_le_bytes(bytes[32..36].try_into().unwrap());

    let roots = seeds("inode");
    let indexed: Vec<&str> = roots
        .iter()
        .filter(|(name, _)| name.starts_with("ext4"))
        .filter(|(_, bytes)| root_flags(bytes) & EXT4_INDEX_FL != 0)
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(
        !indexed.is_empty(),
        "no ext4 root inode in fuzz/corpus/inode has EXT4_INDEX_FL, so the htree decoder \
         is never seeded -- rebuild the corpus with scripts/make-fuzz-corpus.sh",
    );
    let linear = roots
        .iter()
        .any(|(name, bytes)| name.starts_with("ext2") && root_flags(bytes) & EXT4_INDEX_FL == 0);
    assert!(
        linear,
        "the ext2 root in fuzz/corpus/inode is gone or indexed; it is kept linear on purpose",
    );

    let dx_roots: Vec<String> = seeds("dir_block")
        .into_iter()
        .filter(|(_, block)| {
            let Ok(info) = fs_ext4::htree::parse_root_info(block) else {
                return false;
            };
            let Ok((cl, _)) = fs_ext4::htree::parse_root_entries(block) else {
                return false;
            };
            // Eight-byte dx_entry records after the 32-byte header, less
            // an optional 8-byte checksum tail.
            let most = ((block.len() - 32) / 8) as u16;
            info.info_length == 8
                && info.indirect_levels <= 2
                && (most.saturating_sub(1)..=most).contains(&cl.limit)
                && (1..=cl.limit).contains(&cl.count)
        })
        .map(|(name, _)| name)
        .collect();
    assert!(
        !dx_roots.is_empty(),
        "no block in fuzz/corpus/dir_block parses as a dx_root, so the htree decoder is \
         never seeded -- rebuild the corpus with scripts/make-fuzz-corpus.sh",
    );
}
