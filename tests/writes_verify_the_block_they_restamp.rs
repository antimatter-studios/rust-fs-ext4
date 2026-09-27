//! A mutation refuses the directory block it is about to re-stamp.
//!
//! # The defect
//!
//! On a `metadata_csum` volume, a corrupt directory block was refused by
//! `fs_ext4_stat` and **accepted** by thirteen of the fourteen mutating C
//! entry points. Two separate reasons, and closing either alone leaves the
//! hole open:
//!
//! 1. Every mutating method resolved its path with `path::lookup`, the shim
//!    that builds `Checksummer { seed: 0, enabled: false }` — sixteen sites
//!    in `fs.rs`, all inside methods holding `&self` and therefore holding
//!    `self.csum`. `capi::resolve_path` passed the real one, which is where
//!    the asymmetry came from.
//! 2. The write engine's own directory scan is separate from the path walk.
//!    `find_entry_in_dir`, and the emptiness checks in `apply_rename` and
//!    `apply_rmdir`, iterate `dir::DirBlockIter`, which takes no
//!    `Checksummer` at all. That is the scan that finds the entry the
//!    mutation edits.
//!
//! # Why it is worse than a missed check
//!
//! Every one of those paths calls `patch_dir_entry_tail` after editing the
//! block, computing a fresh and **correct** CRC32C over the corrupted
//! contents. So the sequence was: not checked, parsed, edited, written back
//! under a valid checksum. Before the write the damage was detectable by
//! `stat`; after it, nothing in this crate could see it. The defect
//! destroyed the evidence of the thing it failed to check.
//!
//! # What this file does
//!
//! Formats a `metadata_csum` volume in memory, breaks one byte of a
//! directory block's stored tail CRC, and asserts each entry point refuses
//! it — and, separately, that the block is still corrupt afterwards, which
//! is the half that says no re-stamp happened.
//!
//! The acceptance half is deliberate and separate: the same operations on
//! an untouched volume must still succeed. A guard that refused everything
//! would pass every refusal assertion here and make the driver useless.

use fs_ext4::block_io::BlockDevice;
use fs_ext4::error::{Error, Result};
use fs_ext4::extent;
use fs_ext4::fs::Filesystem;
use fs_ext4::mkfs;
use std::sync::{Arc, Mutex};

struct MemDev {
    bytes: Mutex<Vec<u8>>,
    size: u64,
}

impl MemDev {
    fn new(size: u64) -> Arc<Self> {
        Arc::new(Self {
            bytes: Mutex::new(vec![0u8; size as usize]),
            size,
        })
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
        self.size
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

/// A freshly formatted 64 MiB volume with `metadata_csum` on, holding
/// `/holder` with a few files and an empty `/holder/sub`.
///
/// Returns the DEVICE, not a mounted filesystem, and every test mounts its
/// own. That is not tidiness. `Filesystem::mount` wraps the device in a
/// write-through buffer cache, so corrupting the device behind a live mount
/// is served out of the cache and changes nothing the filesystem can see.
/// The first version of this file did exactly that and reported that every
/// entry point accepted the broken block — INCLUDING the read path, which
/// was already verifying and is the control. A clean bill of health
/// produced by the harness rather than by the code, and indistinguishable
/// from a real one except that the control moved too. Corrupt, then mount.
fn populated() -> Arc<dyn BlockDevice> {
    let size: u64 = 64 * 1024 * 1024;
    let dev = MemDev::new(size);
    mkfs::format_filesystem(dev.as_ref(), Some("CSUMGATE"), Some([0x5A; 16]), size, 4096)
        .expect("format_filesystem");
    let dyn_dev: Arc<dyn BlockDevice> = dev.clone();
    {
        let fs = Filesystem::mount(dyn_dev.clone()).expect("mount");
        assert!(
            fs.csum.enabled,
            "the fixture must have metadata_csum on, or every refusal below is vacuous"
        );
        fs.apply_mkdir("/holder", 0o755).expect("mkdir /holder");
        fs.apply_mkdir("/holder/sub", 0o755).expect("mkdir sub");
        for i in 0..4 {
            fs.apply_create(&format!("/holder/f{i}.txt"), 0o644)
                .expect("create");
        }
        // A second directory whose block is never corrupted, so a rename
        // INTO /holder can reach the destination check with everything on
        // the source side verifying.
        fs.apply_mkdir("/other", 0o755).expect("mkdir /other");
        fs.apply_create("/other/movable.txt", 0o644)
            .expect("create movable");
    }
    dyn_dev
}

fn mounted(dev: &Arc<dyn BlockDevice>) -> Filesystem {
    Filesystem::mount(dev.clone()).expect("mount")
}

/// Break `/holder`'s first directory block, on a device with nothing
/// mounted over it. Hands back its inode number and physical block.
fn corrupt_holder(dev: &Arc<dyn BlockDevice>) -> (u32, u64) {
    let (ino, bs, phys) = {
        let fs = mounted(dev);
        let ino = holder_ino(&fs);
        (ino, fs.sb.block_size(), first_block(&fs, dev, ino))
    };
    break_tail_crc(dev, bs, phys);
    (ino, phys)
}

/// Physical block number of a directory's first data block.
fn first_block(fs: &Filesystem, dev: &Arc<dyn BlockDevice>, ino: u32) -> u64 {
    let (inode, _) = fs.read_inode_verified(ino).expect("read inode");
    let bs = fs.sb.block_size();
    extent::map_logical(&inode.block, dev.as_ref(), bs, 0)
        .expect("map_logical")
        .expect("directory has a first block")
}

/// Resolve a path through the READ walk — the one that was already
/// verifying directory blocks — so the tests can use it both to find the
/// block to break and as the control that breaking it was noticed.
fn resolve(fs: &Filesystem, path: &str) -> Result<u32> {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    fs_ext4::path::lookup_with_csum(fs.dev.as_ref(), &fs.sb, &mut reader, path, &fs.csum)
}

fn holder_ino(fs: &Filesystem) -> u32 {
    resolve(fs, "/holder").expect("resolve /holder")
}

/// Flip one bit of the stored tail CRC of the directory block at `phys`.
///
/// THE STORED CHECKSUM, NOT THE CONTENTS. Corrupting an entry would change
/// what a scan finds, so a refusal could come from the parse rather than
/// from the checksum. Changing only the four tail bytes leaves every entry
/// byte-identical, so the ONLY thing that can refuse the block is the tail
/// comparison this fix adds.
fn break_tail_crc(dev: &Arc<dyn BlockDevice>, bs: u32, phys: u64) {
    let mut block = vec![0u8; bs as usize];
    dev.read_at(phys * u64::from(bs), &mut block).expect("read");
    assert!(
        fs_ext4::dir::has_csum_tail(&block),
        "the block must carry an ext4_dir_entry_tail, or there is nothing to break"
    );
    let off = block.len() - 4;
    block[off] ^= 0xFF;
    dev.write_at(phys * u64::from(bs), &block).expect("write");
}

/// Does the block at `phys` still fail verification?
fn still_corrupt(fs: &Filesystem, dev: &Arc<dyn BlockDevice>, ino: u32, phys: u64) -> bool {
    let (inode, _) = fs.read_inode_verified(ino).expect("read inode");
    let bs = fs.sb.block_size();
    let mut block = vec![0u8; bs as usize];
    dev.read_at(phys * u64::from(bs), &mut block).expect("read");
    !fs.csum.verify_dir_entry_tail(ino, inode.generation, &block)
}

fn is_bad_checksum(e: &Error) -> bool {
    matches!(e, Error::BadChecksum { .. })
}

// ---------------------------------------------------------------------------
// The control: the corruption is real, and the READ path already saw it
// ---------------------------------------------------------------------------

/// Without this the refusals below could be produced by a broken fixture —
/// an image that never had a tail, or a `Checksummer` that refuses
/// everything — and would look identical.
#[test]
fn the_read_path_refuses_the_broken_block_and_an_untouched_one_is_fine() {
    let dev = populated();

    // Before: resolving through /holder's block succeeds.
    resolve(&mounted(&dev), "/holder/f0.txt").expect("resolve before the corruption");

    corrupt_holder(&dev);
    let err = resolve(&mounted(&dev), "/holder/f0.txt")
        .expect_err("the read walk must refuse a broken tail");
    assert!(is_bad_checksum(&err), "read walk gave {err:?}");
}

// ---------------------------------------------------------------------------
// The defect: each mutating path must refuse the same block
// ---------------------------------------------------------------------------

macro_rules! refuses {
    ($name:ident, $what:expr, $op:expr) => {
        #[test]
        fn $name() {
            let dev = populated();
            let (ino, phys) = corrupt_holder(&dev);
            let fs = mounted(&dev);

            let op: fn(&Filesystem) -> Result<()> = $op;
            let err = op(&fs).expect_err(concat!(
                $what,
                " must refuse a directory block whose tail does not verify; \
                 accepting it edits the block and re-stamps a valid checksum \
                 over the corruption"
            ));
            assert!(
                is_bad_checksum(&err),
                concat!($what, " gave {err:?}"),
                err = err
            );

            // THE HALF THAT SAYS NO RE-STAMP HAPPENED. A refusal that had
            // already written the block back would satisfy the assertion
            // above and still have laundered the corruption.
            assert!(
                still_corrupt(&fs, &dev, ino, phys),
                concat!(
                    $what,
                    ": the block was rewritten with a fresh valid \
                 checksum despite the refusal"
                )
            );
        }
    };
}

refuses!(unlink_refuses_it, "unlink", |fs| fs
    .apply_unlink("/holder/f0.txt"));
refuses!(rmdir_refuses_it, "rmdir", |fs| fs
    .apply_rmdir("/holder/sub"));
refuses!(mkdir_refuses_it, "mkdir", |fs| fs
    .apply_mkdir("/holder/newdir", 0o755)
    .map(|_| ()));
refuses!(chmod_refuses_it, "chmod", |fs| fs
    .apply_chmod("/holder/f1.txt", 0o600));
refuses!(chown_refuses_it, "chown", |fs| fs.apply_chown(
    "/holder/f1.txt",
    1,
    1
));
refuses!(rename_refuses_it, "rename", |fs| fs.apply_rename(
    "/holder/f2.txt",
    "/holder/f2-renamed.txt",
    false
));
refuses!(link_refuses_it, "link", |fs| fs
    .apply_link("/holder/f3.txt", "/holder/f3-link.txt"));

// THE OTHER NINE ENTRY POINTS (#166). Each reaches a `lookup_with_csum` the
// seven above never execute with a corrupt block underneath, so putting
// `path::lookup` back at any of those sites left this file green.
refuses!(set_flags_refuses_it, "set_flags", |fs| fs
    .apply_set_flags("/holder/f1.txt", 0));
refuses!(setxattr_refuses_it, "setxattr", |fs| fs.apply_setxattr(
    "/holder/f1.txt",
    "user.k",
    b"v"
));
refuses!(removexattr_refuses_it, "removexattr", |fs| fs
    .apply_removexattr("/holder/f1.txt", "user.k"));
refuses!(utimens_refuses_it, "utimens", |fs| fs.apply_utimens(
    "/holder/f1.txt",
    1,
    0,
    1,
    0
));
// One level below `/holder`, so the walk to the new entry's PARENT crosses
// the corrupt block: a name directly in `/holder` resolves the parent
// through the root's block and is refused later, by the duplicate-name
// check, which left `plan_new_inode_in_dir`'s own lookup unwitnessed.
refuses!(create_refuses_it, "create", |fs| fs
    .apply_create("/holder/sub/new.txt", 0o644)
    .map(|_| ()));
refuses!(mknod_refuses_it, "mknod", |fs| fs
    .apply_mknod("/holder/sub/fifo", 0o010644, 0, 0)
    .map(|_| ()));
refuses!(symlink_refuses_it, "symlink", |fs| fs
    .apply_symlink("target", "/holder/sub/link")
    .map(|_| ()));
refuses!(
    replace_file_content_refuses_it,
    "replace_file_content",
    |fs| fs
        .apply_replace_file_content("/holder/f1.txt", b"new")
        .map(|_| ())
);
refuses!(pwrite_refuses_it, "pwrite", |fs| fs
    .apply_pwrite("/holder/f1.txt", 0, b"new")
    .map(|_| ()));

// ---------------------------------------------------------------------------
// The second half of the fix: the emptiness walks, which are a separate scan
// ---------------------------------------------------------------------------

// THE PATH WALK IS NOT THE ONLY SCAN, and the tests above cannot see the
// other one. Each of them corrupts the block holding the entry being
// operated on, so the operation is refused by the lookup or by
// `find_entry_in_dir` and never reaches what follows.
//
// `apply_rmdir` and `apply_rename`'s overwrite branch each walk the TARGET
// directory's own blocks to decide whether it is empty, through
// `dir::DirBlockIter`, which takes no `Checksummer` at all. Re-pointing the
// sixteen lookups does nothing for those. Corrupting the target's block
// rather than its parent's is what reaches them.

/// Break the first block of the directory at `path`.
fn corrupt_dir(dev: &Arc<dyn BlockDevice>, path: &str) -> (u32, u64) {
    let (ino, bs, phys) = {
        let fs = mounted(dev);
        let ino = resolve(&fs, path).unwrap_or_else(|e| panic!("resolve {path}: {e}"));
        (ino, fs.sb.block_size(), first_block(&fs, dev, ino))
    };
    break_tail_crc(dev, bs, phys);
    (ino, phys)
}

/// `rmdir` decides emptiness from the target's own block and then frees it.
/// Unverified, a corrupt block reads as empty or not-empty by accident, and
/// the directory is deleted on the strength of it.
#[test]
fn rmdir_refuses_a_target_whose_own_block_is_corrupt() {
    let dev = populated();
    let (ino, phys) = corrupt_dir(&dev, "/holder/sub");
    let fs = mounted(&dev);

    let err = fs
        .apply_rmdir("/holder/sub")
        .expect_err("rmdir must refuse a target whose own block does not verify");
    assert!(is_bad_checksum(&err), "rmdir gave {err:?}");
    assert!(
        still_corrupt(&fs, &dev, ino, phys),
        "the target's block was rewritten despite the refusal"
    );
}

/// `rename` over an existing directory walks the victim's blocks for the
/// same decision, then overwrites it.
#[test]
fn rename_over_a_directory_refuses_when_the_victim_block_is_corrupt() {
    let dev = populated();
    {
        let fs = mounted(&dev);
        fs.apply_mkdir("/holder/src", 0o755).expect("mkdir src");
    }
    let (ino, phys) = corrupt_dir(&dev, "/holder/sub");
    let fs = mounted(&dev);

    let err = fs
        .apply_rename("/holder/src", "/holder/sub", true)
        .expect_err("rename must refuse to overwrite a directory it could not read");
    assert!(is_bad_checksum(&err), "rename gave {err:?}");
    assert!(
        still_corrupt(&fs, &dev, ino, phys),
        "the victim's block was rewritten despite the refusal"
    );
}

/// The DESTINATION-side existence check, which is a third place the verdict
/// could be discarded.
///
/// `apply_rename` asked `find_entry_in_dir` whether dst already existed and
/// took `.ok()` — mapping a refusal to read the destination's parent block
/// to "dst does not exist". Rename then created the entry in that block and
/// re-stamped it. Everything on the source side verifies here, so this is
/// the only assertion in the file that reaches that branch.
#[test]
fn rename_into_a_corrupt_directory_refuses_rather_than_assuming_the_name_is_free() {
    let dev = populated();
    let (ino, phys) = corrupt_holder(&dev);
    let fs = mounted(&dev);

    let err = fs
        .apply_rename("/other/movable.txt", "/holder/moved.txt", false)
        .expect_err("rename must refuse a destination directory it could not read");
    assert!(is_bad_checksum(&err), "rename gave {err:?}");
    assert!(
        still_corrupt(&fs, &dev, ino, phys),
        "the destination's block was rewritten despite the refusal"
    );
}

/// The MOVED DIRECTORY'S OWN block 0, which a cross-parent rename rewrites
/// to repoint `..` (#322).
///
/// None of the scans above reads it: the source lookup reads the source
/// parent, the existence check reads the destination parent, and the
/// emptiness walk only runs on an overwritten victim. `buffer_update_dotdot`
/// then loaded the block, edited `..` and re-stamped its tail — the htree
/// branch checked the root first, the linear branch checked nothing.
#[test]
fn rename_across_parents_refuses_when_the_moved_directory_block_is_corrupt() {
    let dev = populated();
    let (ino, phys) = corrupt_dir(&dev, "/holder/sub");
    let bs = {
        let fs = mounted(&dev);
        fs.sb.block_size() as usize
    };
    let mut before = vec![0u8; bs];
    dev.read_at(phys * bs as u64, &mut before).expect("read");
    let fs = mounted(&dev);

    let err = fs
        .apply_rename("/holder/sub", "/other/sub", false)
        .expect_err("rename must refuse to repoint `..` in a block it could not verify");
    assert!(is_bad_checksum(&err), "rename gave {err:?}");
    assert!(
        still_corrupt(&fs, &dev, ino, phys),
        "the moved directory's block was rewritten despite the refusal"
    );
    let mut after = vec![0u8; bs];
    dev.read_at(phys * bs as u64, &mut after).expect("read");
    assert_eq!(
        before, after,
        "the moved directory's block changed despite the refusal"
    );
}

/// The acceptance half of the test above: the same move on an untouched
/// volume succeeds, and the block it re-stamped verifies after a remount.
#[test]
fn rename_across_parents_still_works_on_a_volume_this_driver_wrote() {
    let dev = populated();
    let (ino, phys) = {
        let fs = mounted(&dev);
        let ino = resolve(&fs, "/holder/sub").expect("resolve sub");
        (ino, first_block(&fs, &dev, ino))
    };
    mounted(&dev)
        .apply_rename("/holder/sub", "/other/sub", false)
        .expect("cross-parent rename of a directory");

    let fs = mounted(&dev);
    assert_eq!(resolve(&fs, "/other/sub").expect("moved"), ino);
    assert!(
        resolve(&fs, "/holder/sub").is_err(),
        "the old name must be gone"
    );
    assert!(
        !still_corrupt(&fs, &dev, ino, phys),
        "the re-stamped `..` block must verify"
    );
}

// ---------------------------------------------------------------------------
// The acceptance half — without it a guard that refused everything passes
// ---------------------------------------------------------------------------

/// THE DEFEATS FAILING IS NOT ENOUGH. Turning verification on for writes
/// makes the driver refuse any directory block whose tail it cannot
/// reproduce, INCLUDING blocks it wrote itself — and the failure mode of
/// getting that wrong is a volume this driver can read and refuses to
/// write, which reads to a user as data loss.
///
/// So: every operation the tests above assert is refused on a broken block
/// must still succeed on an untouched one, on a volume whose directory
/// blocks were written by this driver rather than by mkfs.
#[test]
fn every_operation_still_works_on_a_volume_this_driver_wrote() {
    let dev = populated();
    let fs = mounted(&dev);
    let ino = holder_ino(&fs);
    let phys = first_block(&fs, &dev, ino);

    // `/holder` and its entries were created by the write path above, so
    // the tail under test is one this driver stamped, not one mkfs did.
    assert!(
        !still_corrupt(&fs, &dev, ino, phys),
        "a block this driver wrote must verify against its own reader"
    );

    fs.apply_unlink("/holder/f0.txt").expect("unlink");
    fs.apply_rmdir("/holder/sub").expect("rmdir");
    fs.apply_mkdir("/holder/newdir", 0o755).expect("mkdir");
    fs.apply_chmod("/holder/f1.txt", 0o600).expect("chmod");
    fs.apply_chown("/holder/f1.txt", 1, 1).expect("chown");
    fs.apply_rename("/holder/f2.txt", "/holder/f2-renamed.txt", false)
        .expect("rename");
    fs.apply_link("/holder/f3.txt", "/holder/f3-link.txt")
        .expect("link");

    // And the volume is still self-consistent afterwards: re-mounting and
    // re-reading exercises the read-side verifier over everything the write
    // path just re-stamped.
    let remounted = Filesystem::mount(dev.clone()).expect("remount");
    for present in [
        "/holder/newdir",
        "/holder/f2-renamed.txt",
        "/holder/f3-link.txt",
    ] {
        resolve(&remounted, present)
            .unwrap_or_else(|e| panic!("{present} should resolve after a remount: {e}"));
    }
    for gone in ["/holder/f0.txt", "/holder/sub", "/holder/f2.txt"] {
        assert!(
            resolve(&remounted, gone).is_err(),
            "{gone} was removed and must not resolve"
        );
    }
}
