//! Seeded random sequences of write operations leave volumes e2fsck accepts.
//!
//! Each targeted test pins one shape. This one looks for the shapes nobody
//! wrote a test for: it mixes every public mutation (data writes, content
//! replacement, truncate both ways, preallocation, hole punching, xattrs,
//! links, renames, unlinks, directories, symlinks, device nodes, metadata
//! changes and bulk creates) over a handful of paths that can each be any
//! kind of inode, and runs `e2fsck -fn` every 25 operations. A refused
//! operation is fine; a volume e2fsck rejects is not.
//!
//! A run like this found #242, #245, #251 and #253. The sequence depends
//! only on the seed, so a failure prints the operations since the last clean
//! check, and the same seed reproduces it.
//!
//! Three geometries: 4 KiB extent-mapped (the default), 1 KiB blocks, and a
//! block-mapped volume without extents. the e2fsprogs tools run in the harness VM;
//! the test fails if it is missing, rather than passing without checking
//! anything.

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use std::sync::Arc;

const STEPS: u64 = 150;
const CHECK_EVERY: u64 = 25;
const SEEDS: [u64; 2] = [1, 2];

/// xorshift64: small, deterministic, and the same on every platform.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// e2fsck's complaint, or `None` for a clean volume. A report that is not
/// a verdict — e2fsck could not open the image, or skipped the check —
/// fails here rather than counting as either.
fn e2fsck(image: &str) -> Option<String> {
    let judged = fs_ext4_test_support::oracle("e2fsck")
        .args(["-fn", image])
        .judged();
    match judged.verdict {
        fs_ext4_test_support::Verdict::Clean => None,
        fs_ext4_test_support::Verdict::Findings(said) => Some(said),
        fs_ext4_test_support::Verdict::NotAVerdict(why) => {
            panic!("{}: not a verdict on {image}: {why}", judged.call)
        }
    }
}

fn mount(image: &str) -> Filesystem {
    Filesystem::mount(Arc::new(FileDevice::open_rw(image).unwrap())).unwrap()
}

/// One operation, chosen by `rng`, described for the failure message.
fn operate(fs: &Filesystem, rng: &mut Rng, step: u64, seed: u64) -> String {
    const PATHS: [&str; 9] = ["/a", "/b", "/c", "/d/x", "/d/y", "/e", "/d/s", "/f/g", "/f"];
    let n = PATHS[rng.below(PATHS.len() as u64) as usize];
    let m = PATHS[rng.below(PATHS.len() as u64) as usize];
    let ino = || {
        let mut read = |i: u32| fs.read_inode_verified(i).map(|(inode, _)| inode);
        fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut read, n).ok()
    };
    let byte = (step % 250) as u8 + 1;
    let (what, result): (String, Result<(), fs_ext4::Error>) = match rng.below(22) {
        0 => (format!("create {n}"), fs.apply_create(n, 0o644).map(|_| ())),
        1 | 2 => {
            let (off, len) = (rng.below(200_000), 1 + rng.below(60_000) as usize);
            (
                format!("pwrite {n} {off} {len}"),
                fs.apply_pwrite(n, off, &vec![byte; len]).map(|_| ()),
            )
        }
        3 => {
            let len = rng.below(50_000) as usize;
            (
                format!("replace {n} {len}"),
                fs.apply_replace_file_content(n, &vec![7; len]).map(|_| ()),
            )
        }
        4 => match ino() {
            Some(i) => {
                let size = rng.below(150_000);
                (
                    format!("truncate_shrink {n} {size}"),
                    fs.apply_truncate_shrink(i, size),
                )
            }
            None => (format!("truncate_shrink {n} (absent)"), Ok(())),
        },
        5 => match ino() {
            Some(i) => {
                let size = rng.below(300_000);
                (
                    format!("truncate_grow {n} {size}"),
                    fs.apply_truncate_grow(i, size),
                )
            }
            None => (format!("truncate_grow {n} (absent)"), Ok(())),
        },
        6 => match ino() {
            Some(i) => {
                let (off, len) = (rng.below(200_000), 1 + rng.below(80_000));
                (
                    format!("fallocate {n} {off} {len}"),
                    fs.apply_fallocate_keep_size(i, off, len),
                )
            }
            None => (format!("fallocate {n} (absent)"), Ok(())),
        },
        7 => match ino() {
            Some(i) => {
                let (off, len) = (rng.below(200_000), 1 + rng.below(80_000));
                (
                    format!("punch {n} {off} {len}"),
                    fs.apply_fallocate_punch_hole(i, off, len),
                )
            }
            None => (format!("punch {n} (absent)"), Ok(())),
        },
        8 => (format!("unlink {n}"), fs.apply_unlink(n)),
        9 => {
            let replace = rng.below(2) == 0;
            (
                format!("rename {n} {m} replace={replace}"),
                fs.apply_rename(n, m, replace),
            )
        }
        10 => {
            let (key, len) = (rng.below(4), rng.below(900) as usize);
            (
                format!("setxattr {n} user.k{key} {len}"),
                fs.apply_setxattr(n, &format!("user.k{key}"), &vec![b'z'; len]),
            )
        }
        11 => {
            let key = rng.below(4);
            (
                format!("removexattr {n} user.k{key}"),
                fs.apply_removexattr(n, &format!("user.k{key}")),
            )
        }
        12 => (format!("link {n} {m}"), fs.apply_link(n, m)),
        13 => (format!("mkdir {n}"), fs.apply_mkdir(n, 0o755).map(|_| ())),
        14 => (format!("rmdir {n}"), fs.apply_rmdir(n)),
        15 => {
            let target = "t".repeat(1 + rng.below(120) as usize);
            (
                format!("symlink {} {n}", target.len()),
                fs.apply_symlink(&target, n).map(|_| ()),
            )
        }
        16 => (
            format!("mknod {n}"),
            fs.apply_mknod(n, 0o020644, 4, 1).map(|_| ()),
        ),
        17 => {
            let mode = rng.below(0o7777) as u16;
            (format!("chmod {n} {mode:o}"), fs.apply_chmod(n, mode))
        }
        18 => {
            let (uid, gid) = (rng.below(3000) as u32, rng.below(3000) as u32);
            (
                format!("chown {n} {uid} {gid}"),
                fs.apply_chown(n, uid, gid),
            )
        }
        19 => {
            let t = rng.below(5_000_000_000) as i64;
            (
                format!("utimens {n} {t}"),
                fs.apply_utimens(n, t, 5, t + 1, 7),
            )
        }
        20 => {
            let target = "u".repeat(2000 + rng.below(4000) as usize);
            (
                format!("symlink {} {n}", target.len()),
                fs.apply_symlink(&target, n).map(|_| ()),
            )
        }
        _ => {
            let count = 1 + rng.below(40);
            for k in 0..count {
                let _ = fs.apply_create(&format!("/d/many_{seed}_{step}_{k}"), 0o644);
            }
            (format!("create {count} files in /d"), Ok(()))
        }
    };
    match result {
        Ok(()) => format!("{step}: {what} -> ok"),
        Err(e) => format!("{step}: {what} -> {e:?}"),
    }
}

#[test]
fn random_operation_sequences_leave_volumes_e2fsck_accepts() {
    let geometries: [(&str, &[&str]); 3] = [
        ("4k", &["-b", "4096"]),
        ("1k", &["-b", "1024"]),
        ("blockmap", &["-b", "4096", "-O", "^extent,^64bit"]),
    ];
    for (geometry, args) in geometries {
        for seed in SEEDS {
            let image = fs_ext4_test_support::temp_path!(
                "fs_ext4_random_ops_{geometry}_{seed}_{}.img",
                std::process::id()
            );
            std::fs::File::create(&image)
                .and_then(|f| f.set_len(64 * 1024 * 1024))
                .unwrap();
            let out = fs_ext4_test_support::oracle("mkfs.ext4")
                .args(["-q", "-F"])
                .args(args)
                .arg(&image)
                .output();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );

            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut since_check: Vec<String> = Vec::new();
            let mut fs = mount(&image);
            let _ = fs.apply_mkdir("/d", 0o755);
            for step in 0..STEPS {
                since_check.push(operate(&fs, &mut rng, step, seed));
                if step % CHECK_EVERY == CHECK_EVERY - 1 {
                    drop(fs);
                    if let Some(complaint) = e2fsck(&image) {
                        panic!(
                            "[{geometry} seed {seed}] e2fsck rejected the volume after step \
                             {step}:\n{complaint}\noperations since the last clean check:\n{}",
                            since_check.join("\n")
                        );
                    }
                    since_check.clear();
                    fs = mount(&image);
                }
            }
            drop(fs);
            let _ = std::fs::remove_file(&image);
        }
    }
}
