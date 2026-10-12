//! A lazy read-write mount over a log the kernel committed and never
//! checkpointed: nothing is written before replay, and after it a directory
//! and the entries inside it land in a group still `INODE_UNINIT` (#506).
//!
//! This is the sequence that corrupted a volume in the field. A Linux system
//! lost power with its log holding committed transactions whose blocks had
//! not reached their home locations. The volume was then mounted read-write
//! with replay deferred, and two things went wrong. The first commit was
//! written at the head of the log and marked the journal clean, so the
//! kernel's transactions were never replayed (#409). And a new directory was
//! placed in a group still flagged `INODE_UNINIT` whose flag and bitmap were
//! left as they were, so the file created inside it was handed the same
//! inode (#41).
//!
//! The volume is `mkfs.ext4`'s and the log is the kernel's. In the harness VM
//! the kernel writes a tree and fsyncs every file and directory. An fsync
//! commits the log and writes the file's data, but leaves the metadata the
//! log describes in the kernel's cache rather than at its home location. The
//! loop device's backing file is copied at that moment, while still mounted:
//! the bytes a power cut would have left. That copy is the volume under test.
//!
//! `e2fsck -fn` then judges the result, and the kernel, mounting it, must
//! read back both its own tree and the entries this crate added.

use fs_ext4::bgd::BgdFlags;
use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use fs_ext4::Error;
use fs_ext4_test_support::{guest_quote, sha256_hex};
use std::collections::BTreeSet;
use std::sync::Arc;

/// The kernel's top-level directories. Three of eight groups, so most groups
/// are still `INODE_UNINIT` when this crate allocates.
const KERNEL_DIRS: [&str; 3] = ["alpha", "bravo", "charlie"];
/// Files per kernel directory; file `n` holds `n * FILE_UNIT` bytes of digit `n`.
const FILES_PER_DIR: u8 = 3;
const FILE_UNIT: usize = 6000;
/// What this crate writes into the file it creates in the new directory.
const NEW_FILE_BYTES: &[u8] = b"0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9\n";

fn kernel_file(n: u8) -> Vec<u8> {
    vec![b'0' + n; n as usize * FILE_UNIT]
}

fn resolve(fs: &Filesystem, path: &str) -> fs_ext4::Result<u32> {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(inode, _)| inode);
    fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, path)
}

/// A `mkfs.ext4` volume of eight 32 MiB groups, inode tables written out so
/// no background initialisation adds transactions of its own.
fn format(image: &str) {
    std::fs::File::create(image)
        .and_then(|f| f.set_len(256 * 1024 * 1024))
        .unwrap();
    let out = fs_ext4_test_support::oracle("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-b",
            "4096",
            "-g",
            "8192",
            "-N",
            "4096",
            "-I",
            "256",
            "-O",
            "metadata_csum,64bit",
            "-E",
            "lazy_itable_init=0,lazy_journal_init=0",
            image,
        ])
        .output();
    assert_eq!(
        out.status.code(),
        Some(0),
        "mkfs.ext4: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Have the kernel write its tree onto `image` and copy what a power cut
/// would have left to `crash`.
fn crash_image(image: &str, crash: &str) {
    let script = format!(
        r#"
# Write back everything already dirty in the guest first. Writeback that
# starts for someone else's pages while this tree is in the cache takes the
# tree's metadata home with it, and the copy below is no crash image.
sync
cd "$MNT"
for dir in {dirs}; do
    mkdir "$dir"
    for n in $(seq 1 {files}); do
        head -c $((n * {unit})) /dev/zero | tr '\0' "$n" > "$dir/file$n"
    done
done
# fsync, not sync: syncfs would also write the metadata home.
sync {dirs_glob} {dirs} .
back="$(losetup --noheadings --output BACK-FILE "$(findmnt --noheadings --output SOURCE "$MNT")")"
cp --sparse=always "$back" {crash}
"#,
        dirs = KERNEL_DIRS.join(" "),
        dirs_glob = KERNEL_DIRS.map(|d| format!("{d}/*")).join(" "),
        files = FILES_PER_DIR,
        unit = FILE_UNIT,
        crash = guest_quote(crash),
    );
    let out = fs_ext4_test_support::guest_kernel_write(image, &script);
    assert!(
        out.status.success(),
        "the kernel could not write the tree ({:?}):\n{}{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_lazy_mount_writes_into_an_uninit_group_only_after_replaying_the_kernels_log() {
    let image = fs_ext4_test_support::temp_path!("fs_ext4_506_{}.img", std::process::id());
    let crash = format!("{image}.crash");
    format(&image);
    crash_image(&image, &crash);

    let mut fs = Filesystem::mount_lazy(Arc::new(FileDevice::open_rw(&crash).unwrap()))
        .expect("lazy read-write mount");

    // The premise: the kernel's log is committed and its tree is not yet at
    // home, so only a replay can bring it back.
    let log = fs_ext4::jbd2::read_superblock(&fs)
        .unwrap()
        .expect("the volume has a journal");
    assert!(
        !log.is_clean(),
        "the kernel's log was already checkpointed when the copy was taken"
    );
    assert!(
        resolve(&fs, "/alpha").is_err(),
        "the kernel's tree is already at its home location, so replay proves nothing"
    );

    // A write before replay would be committed at the head of the log and
    // discard the kernel's transactions.
    match fs.apply_mkdir("/.fseventsd", 0o700) {
        Err(Error::JournalNotReplayed) => {}
        other => panic!("a write before replay must be refused, got {other:?}"),
    }
    let after = fs_ext4::jbd2::read_superblock(&fs).unwrap().unwrap();
    assert_eq!(
        (after.start, after.sequence),
        (log.start, log.sequence),
        "the refused write still moved the log"
    );

    let replayed = fs.replay_journal_if_dirty().expect("replay");
    assert!(replayed > 0, "replay found nothing to apply");
    assert!(fs_ext4::jbd2::read_superblock(&fs)
        .unwrap()
        .unwrap()
        .is_clean());

    let ipg = fs.sb.inodes_per_group;
    let uninit: Vec<bool> = fs
        .groups
        .iter()
        .map(|g| g.flags().contains(BgdFlags::INODE_UNINIT))
        .collect();

    let dir = fs.apply_mkdir("/.fseventsd", 0o700).expect("mkdir");
    let group = ((dir - 1) / ipg) as usize;
    assert!(
        uninit[group],
        "/.fseventsd (inode {dir}) went to group {group}, which was already initialised; \
         the test needs a group still INODE_UNINIT (uninit groups: {uninit:?})"
    );
    let file = fs
        .apply_create("/.fseventsd/fseventsd-uuid", 0o600)
        .expect("create in the new directory");
    fs.apply_pwrite("/.fseventsd/fseventsd-uuid", 0, NEW_FILE_BYTES)
        .expect("write");
    let sub = fs
        .apply_mkdir("/.fseventsd/sub", 0o755)
        .expect("mkdir in the new directory");
    let top = fs
        .apply_create("/top", 0o644)
        .expect("create in an initialised group");
    let inodes = [dir, file, sub, top];
    assert_eq!(
        inodes.iter().collect::<BTreeSet<_>>().len(),
        inodes.len(),
        "one inode was handed out twice: {inodes:?}"
    );
    drop(fs);

    fs_ext4_test_support::assert_e2fsck_clean(
        &crash,
        "after replay and writes into an uninit group",
    );

    let report =
        fs_ext4_test_support::guest_kernel_report(&crash, "the kernel reads the volume back");
    let get = |kind: &str, path: &str| report.get(&(kind.to_string(), path.to_string())).cloned();
    for dir in KERNEL_DIRS {
        assert_eq!(get("type", dir).as_deref(), Some("directory"), "{dir}");
        for n in 1..=FILES_PER_DIR {
            let path = format!("{dir}/file{n}");
            assert_eq!(
                get("sha256", &path),
                Some(sha256_hex(&kernel_file(n))),
                "the kernel's {path} did not survive replay"
            );
        }
    }
    assert_eq!(get("type", ".fseventsd").as_deref(), Some("directory"));
    assert_eq!(get("mode", ".fseventsd").as_deref(), Some("700"));
    assert_eq!(
        get("type", ".fseventsd/fseventsd-uuid").as_deref(),
        Some("regular-file")
    );
    assert_eq!(
        get("sha256", ".fseventsd/fseventsd-uuid"),
        Some(sha256_hex(NEW_FILE_BYTES))
    );
    assert_eq!(
        get("mode", ".fseventsd/fseventsd-uuid").as_deref(),
        Some("600")
    );
    assert_eq!(get("type", ".fseventsd/sub").as_deref(), Some("directory"));
    assert_eq!(get("type", "top").as_deref(), Some("regular-empty-file"));
}
