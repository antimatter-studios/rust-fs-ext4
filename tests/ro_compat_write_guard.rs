//! A feature this driver does not maintain permits reading and refuses
//! writing.
//!
//! `RO_COMPAT` states one rule with two halves: an implementation that
//! does not know the bit may READ the filesystem and must not WRITE it.
//! This driver enforced the first half and not the second — its own
//! `check_mountable` comment says "mounted read-only", which stopped
//! being true a long time ago — so a volume carrying such a bit was
//! mounted writable, and a create updated what the driver knows about
//! and silently left the rest.
//!
//! The case to picture is `QUOTA`, which is not hypothetical: the
//! string "quota" appears in `src/features.rs` and nowhere else in the
//! crate, so a create on a quota-enabled volume charged nobody for the
//! file and left counters that no longer describe the filesystem.

use fs_ext4::checksum::linux_crc32c;
use fs_ext4::features::RoCompat;
use fs_ext4::{Error, Filesystem};
use std::sync::Arc;

const IMAGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

/// The superblock starts 1024 bytes in; `s_feature_ro_compat` is at
/// 0x64 within it, and `s_checksum` at 0x3FC.
const SB_AT: usize = 1024;
const RO_COMPAT_AT: usize = SB_AT + 0x64;
const CSUM_AT: usize = SB_AT + 0x3FC;

/// A writable in-memory device, so a fixture can be edited without
/// touching the file on disk.
struct MemDev {
    bytes: std::sync::Mutex<Vec<u8>>,
}

impl MemDev {
    fn arc(bytes: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            bytes: std::sync::Mutex::new(bytes),
        })
    }
}

impl fs_ext4::block_io::BlockDevice for MemDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_ext4::Result<()> {
        let b = self.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start + buf.len();
        if end > b.len() {
            return Err(Error::OutOfBounds);
        }
        buf.copy_from_slice(&b[start..end]);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.bytes.lock().unwrap().len() as u64
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_ext4::Result<()> {
        let mut b = self.bytes.lock().unwrap();
        let start = offset as usize;
        if start + buf.len() > b.len() {
            return Err(Error::OutOfBounds);
        }
        b[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

/// A copy of the fixture with `bits` added to `s_feature_ro_compat` and
/// the superblock checksum restored, so the volume is well-formed and
/// differs from the original only in the feature mask.
fn fixture_with_ro_compat(bits: u32) -> Arc<MemDev> {
    let mut bytes = std::fs::read(IMAGE).expect("read the fixture");
    let existing = u32::from_le_bytes(bytes[RO_COMPAT_AT..RO_COMPAT_AT + 4].try_into().unwrap());
    let updated = existing | bits;
    bytes[RO_COMPAT_AT..RO_COMPAT_AT + 4].copy_from_slice(&updated.to_le_bytes());

    // METADATA_CSUM is set on this fixture, so the superblock carries a
    // checksum over everything before it. Leave it stale and the mount
    // fails for the wrong reason entirely.
    let csum = linux_crc32c(!0, &bytes[SB_AT..CSUM_AT]);
    bytes[CSUM_AT..CSUM_AT + 4].copy_from_slice(&csum.to_le_bytes());
    MemDev::arc(bytes)
}

/// The bits worth testing, and why each one.
fn cases() -> Vec<(&'static str, u32)> {
    vec![
        // Known, tolerated for reading, and NOT maintained: nothing in
        // this crate touches the quota inodes.
        ("quota", RoCompat::QUOTA.bits()),
        // Known and not maintained either — the orphan file is read and
        // not kept up to date.
        ("orphan_present", RoCompat::ORPHAN_PRESENT.bits()),
        // Nothing at all yet, which is what a future feature looks like
        // from here and the case the rule exists for.
        ("an unassigned bit", 1 << 28),
    ]
}

/// Every one of them still mounts and reads.
///
/// The mount path must not get stricter than the format. Refusing to
/// read a volume because of a `RO_COMPAT` bit would lock a user out of
/// data that is perfectly readable, which is the opposite of what the
/// bit says.
#[test]
fn an_unmaintained_ro_compat_bit_still_reads() {
    for (name, bits) in cases() {
        let dev = fixture_with_ro_compat(bits);
        let fs = Filesystem::mount(dev)
            .unwrap_or_else(|e| panic!("{name}: the volume must still be readable, got {e:?}"));
        // Reading the root inode is enough to say the volume is
        // readable: it goes through the superblock, the group
        // descriptors and the inode table, which is every structure the
        // mount just validated.
        let (inode, _raw) = fs
            .read_inode_verified(2)
            .unwrap_or_else(|e| panic!("{name}: reading the root inode failed: {e:?}"));
        assert!(
            inode.size > 0,
            "{name}: the root inode came back describing nothing"
        );
    }
}

/// And every one of them refuses a write, naming the bit.
#[test]
fn an_unmaintained_ro_compat_bit_refuses_a_write() {
    for (name, bits) in cases() {
        let dev = fixture_with_ro_compat(bits);
        let fs = Filesystem::mount(dev).expect("mount");
        match fs.apply_create("/newfile.txt", 0o644) {
            Err(Error::UnsupportedRoCompat(reported)) => {
                assert_eq!(
                    reported & bits,
                    bits,
                    "{name}: the refusal should name the bit that caused it"
                );
            }
            Err(other) => panic!("{name}: wrong refusal {other:?}"),
            Ok(_) => panic!("{name}: a create must be refused on this volume"),
        }
    }
}

/// An ordinary volume is not caught by the guard.
///
/// This is how the check is most likely to go wrong: a mask that is too
/// tight refuses volumes `mke2fs` makes with its defaults, and the
/// failure would look like the driver breaking rather than a feature
/// being refused.
#[test]
fn a_default_volume_still_writes() {
    let bytes = std::fs::read(IMAGE).expect("read the fixture");
    let fs = Filesystem::mount(MemDev::arc(bytes)).expect("mount");
    fs.apply_create("/guard_smoke.txt", 0o644)
        .expect("an ordinary volume must still accept a create");
}

/// The whole device, to prove a refusal wrote nothing at all.
fn snapshot(dev: &MemDev) -> Vec<u8> {
    dev.bytes.lock().unwrap().clone()
}

/// `write_inode_raw` refuses too, and writes nothing (#323).
///
/// It is public, writes straight to the device, and checked only the
/// length, so an outside caller wrote an inode onto a volume every
/// `apply_*` refuses.
#[test]
fn write_inode_raw_refuses_an_unmaintained_ro_compat_volume() {
    for (name, bits) in cases() {
        let dev = fixture_with_ro_compat(bits);
        let fs = Filesystem::mount(dev.clone()).expect("mount");
        let raw = fs.read_inode_raw(2).expect("read the root inode");
        let before = snapshot(&dev);
        match fs.write_inode_raw(2, &raw) {
            Err(Error::UnsupportedRoCompat(reported)) => assert_eq!(
                reported & bits,
                bits,
                "{name}: the refusal should name the bit that caused it"
            ),
            Err(other) => panic!("{name}: wrong refusal {other:?}"),
            Ok(()) => panic!("{name}: write_inode_raw must be refused on this volume"),
        }
        assert!(
            snapshot(&dev) == before,
            "{name}: a refused write_inode_raw changed the device"
        );
    }
}

/// `s_state` straight off the device.
fn on_disk_state(dev: &MemDev) -> u16 {
    let b = dev.bytes.lock().unwrap();
    u16::from_le_bytes([b[SB_AT + 0x3A], b[SB_AT + 0x3B]])
}

/// `EXT4_VALID_FS` in `s_state`: the volume reads as cleanly unmounted.
const VALID_FS: u16 = 0x0001;

/// A `write_inode_raw` on an ordinary volume marks it not clean, as every
/// other write does (#323). Unmarked, a crash after it left a modified
/// volume claiming to have been put away properly.
#[test]
fn write_inode_raw_marks_the_volume_not_clean() {
    let dev = MemDev::arc(std::fs::read(IMAGE).expect("read the fixture"));
    assert_ne!(on_disk_state(&dev) & VALID_FS, 0, "fixture: clean");
    let fs = Filesystem::mount(dev.clone()).expect("mount");
    let raw = fs.read_inode_raw(2).expect("read the root inode");
    fs.write_inode_raw(2, &raw)
        .expect("an ordinary volume must accept the write");
    assert_eq!(
        on_disk_state(&dev) & VALID_FS,
        0,
        "a volume written through write_inode_raw must not read as clean"
    );
}

/// Every public method that writes, by name. The sweep for #323 found
/// `write_inode_raw` alone among them without the guard; this list and
/// the guard below keep a new one from arriving without it.
const PUBLIC_WRITERS: &[&str] = &[
    "write_inode_raw",
    "apply_truncate_shrink",
    "apply_truncate_grow",
    "apply_fallocate_keep_size",
    "apply_fallocate_punch_hole",
    "apply_fallocate_zero_range",
    "apply_chmod",
    "apply_chown",
    "apply_set_flags",
    "apply_removexattr",
    "apply_setxattr",
    "apply_utimens",
    "apply_unlink",
    "apply_create",
    "apply_mknod",
    "apply_symlink",
    "apply_replace_file_content",
    "apply_pwrite",
    "apply_mkdir",
    "apply_link",
    "apply_rename",
    "apply_rmdir",
    "audit_repair",
];

/// Each public writer, called on `fs`, returning only its verdict.
fn call(fs: &Filesystem, writer: &str) -> fs_ext4::Result<()> {
    let ino = 2;
    match writer {
        "write_inode_raw" => fs.write_inode_raw(ino, &fs.read_inode_raw(ino)?),
        "apply_truncate_shrink" => fs.apply_truncate_shrink(ino, 0),
        "apply_truncate_grow" => fs.apply_truncate_grow(ino, 1 << 20),
        "apply_fallocate_keep_size" => fs.apply_fallocate_keep_size(ino, 0, 4096),
        "apply_fallocate_punch_hole" => fs.apply_fallocate_punch_hole(ino, 0, 4096),
        "apply_fallocate_zero_range" => fs.apply_fallocate_zero_range(ino, 0, 4096),
        "apply_chmod" => fs.apply_chmod("/", 0o700),
        "apply_chown" => fs.apply_chown("/", 1, 1),
        "apply_set_flags" => fs.apply_set_flags("/", 0),
        "apply_removexattr" => fs.apply_removexattr("/", "user.x"),
        "apply_setxattr" => fs.apply_setxattr("/", "user.x", b"v"),
        "apply_utimens" => fs.apply_utimens("/", 1, 0, 1, 0),
        "apply_unlink" => fs.apply_unlink("/f"),
        "apply_create" => fs.apply_create("/f", 0o644).map(drop),
        "apply_mknod" => fs.apply_mknod("/n", 0o010_644, 0, 0).map(drop),
        "apply_symlink" => fs.apply_symlink("/t", "/s").map(drop),
        "apply_replace_file_content" => fs.apply_replace_file_content("/f", b"x").map(drop),
        "apply_pwrite" => fs.apply_pwrite("/f", 0, b"x").map(drop),
        "apply_mkdir" => fs.apply_mkdir("/d", 0o755).map(drop),
        "apply_link" => fs.apply_link("/f", "/g"),
        "apply_rename" => fs.apply_rename("/f", "/g", false),
        "apply_rmdir" => fs.apply_rmdir("/d"),
        "audit_repair" => fs.audit_repair(u32::MAX, u32::MAX, true).map(drop),
        other => panic!("no call for public writer {other}"),
    }
}

/// Every public writer refuses a `QUOTA` volume and leaves every byte of
/// it as it was -- including `s_state`, which a refusal must not clear.
#[test]
fn every_public_writer_refuses_and_writes_nothing() {
    let quota = RoCompat::QUOTA.bits();
    for writer in PUBLIC_WRITERS {
        let dev = fixture_with_ro_compat(quota);
        let fs = Filesystem::mount(dev.clone()).expect("mount");
        let before = snapshot(&dev);
        match call(&fs, writer) {
            Err(Error::UnsupportedRoCompat(reported)) => assert_eq!(reported & quota, quota),
            other => panic!("{writer}: expected UnsupportedRoCompat, got {other:?}"),
        }
        assert!(
            snapshot(&dev) == before,
            "{writer}: a refused write changed the device"
        );
    }

    // Orphan recovery runs on every mount, so it declines rather than
    // failing it -- and still writes nothing.
    let dev = fixture_with_ro_compat(quota);
    let fs = Filesystem::mount(dev.clone()).expect("mount");
    let before = snapshot(&dev);
    assert_eq!(fs.recover_orphans().expect("declines, not fails"), 0);
    assert!(
        snapshot(&dev) == before,
        "recover_orphans changed the device"
    );
}

/// Every `pub fn apply_*` / `pub fn write_*` on `Filesystem` is in
/// [`PUBLIC_WRITERS`], so a new public writer cannot skip the test above.
#[test]
fn every_public_writer_is_listed() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/fs.rs"))
        .expect("src/fs.rs is readable");
    let found: Vec<&str> = src
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("pub fn "))
        .map(|rest| rest.split(['(', '<']).next().unwrap_or(""))
        .filter(|name| name.starts_with("apply_") || name.starts_with("write_"))
        .collect();
    assert!(
        found.len() > 20,
        "the scan found too few writers: {found:?}"
    );
    for name in &found {
        assert!(
            PUBLIC_WRITERS.contains(name),
            "public writer {name} is not covered by every_public_writer_refuses_and_writes_nothing"
        );
    }
}
