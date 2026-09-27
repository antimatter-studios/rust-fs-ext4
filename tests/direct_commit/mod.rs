//! The volume and the fault-injecting device shared by the #319 tests:
//! `direct_commit_failure.rs` (unit tier) and
//! `direct_commit_failure_oracle.rs` (the same scenario, read back by the
//! independent checker in the harness VM).
#![allow(dead_code)]

use fs_ext4::bgd::BgdFlags;
use fs_ext4::block_io::BlockDevice;
use fs_ext4::error::{Error, Result};
use fs_ext4::fs::Filesystem;
use fs_ext4::mkfs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub const BLOCK_SIZE: u32 = 2048;
pub const IMAGE_BYTES: u64 = 128 * 1024 * 1024;
pub const NEVER: usize = usize::MAX;

/// An in-memory device that fails exactly one `write_at` -- the `fail_at`-th,
/// counting from zero once armed -- and records the byte offset of every
/// write it is asked for.
pub struct Faulty {
    bytes: Mutex<Vec<u8>>,
    writes: Mutex<Vec<u64>>,
    seen: AtomicUsize,
    fail_at: AtomicUsize,
}

impl Faulty {
    pub fn new(size: u64) -> Arc<Self> {
        Arc::new(Self {
            bytes: Mutex::new(vec![0u8; size as usize]),
            writes: Mutex::new(Vec::new()),
            seen: AtomicUsize::new(0),
            fail_at: AtomicUsize::new(NEVER),
        })
    }

    /// Start counting writes from zero, failing the `fail_at`-th.
    pub fn arm(&self, fail_at: usize) {
        self.writes.lock().unwrap().clear();
        self.seen.store(0, Ordering::SeqCst);
        self.fail_at.store(fail_at, Ordering::SeqCst);
    }

    pub fn writes(&self) -> Vec<u64> {
        self.writes.lock().unwrap().clone()
    }

    /// The whole volume as it stands.
    pub fn bytes_snapshot(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }

    pub fn block(&self, block: u64) -> Vec<u8> {
        let bs = BLOCK_SIZE as usize;
        let at = block as usize * bs;
        self.bytes.lock().unwrap()[at..at + bs].to_vec()
    }
}

impl BlockDevice for Faulty {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let b = self.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start + buf.len();
        assert!(end <= b.len(), "read past EOF");
        buf.copy_from_slice(&b[start..end]);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        self.writes.lock().unwrap().push(offset);
        if self.seen.fetch_add(1, Ordering::SeqCst) == self.fail_at.load(Ordering::SeqCst) {
            return Err(Error::Corrupt("injected write failure"));
        }
        let mut b = self.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start + buf.len();
        assert!(end <= b.len(), "write past EOF");
        b[start..end].copy_from_slice(buf);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        IMAGE_BYTES
    }
    fn is_writable(&self) -> bool {
        true
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// What an untouched group's bitmap blocks hold after a real `mkfs.ext4`:
/// bytes no reader may look at while the uninit flag stands. Every bit set
/// is the worst legal content, and a value no initialised bitmap here holds.
pub const GARBAGE: u8 = 0xFF;

/// Put group `gi` into the state `mkfs.ext4` leaves an untouched group in:
/// both uninit bits set and the bitmaps holding [`GARBAGE`].
pub fn mark_group_uninit(dev: &Faulty, fs: &Filesystem, gi: usize) {
    const INODE_UNINIT: u16 = 0x0001;
    const BLOCK_UNINIT: u16 = 0x0002;

    let bs = fs.sb.block_size() as u64;
    let garbage = vec![GARBAGE; bs as usize];
    for block in [fs.groups[gi].block_bitmap, fs.groups[gi].inode_bitmap] {
        dev.write_at(block * bs, &garbage).expect("scribble bitmap");
    }

    let desc_size = fs.sb.desc_size as usize;
    let desc_at = descriptor_offset(fs, gi);
    let mut desc = vec![0u8; desc_size];
    dev.read_at(desc_at, &mut desc).expect("read bgd");
    let flags = u16::from_le_bytes(desc[0x12..0x14].try_into().unwrap());
    desc[0x12..0x14].copy_from_slice(&(flags | INODE_UNINIT | BLOCK_UNINIT).to_le_bytes());
    assert!(
        fs.csum.enabled,
        "the default format checksums its descriptors"
    );
    desc[0x1E..0x20].copy_from_slice(&[0, 0]);
    let c = fs_ext4::checksum::linux_crc32c(fs.csum.seed, &(gi as u32).to_le_bytes());
    let c = fs_ext4::checksum::linux_crc32c(c, &desc);
    desc[0x1E..0x20].copy_from_slice(&(c as u16).to_le_bytes());
    dev.write_at(desc_at, &desc).expect("write bgd");
}

pub fn descriptor_offset(fs: &Filesystem, gi: usize) -> u64 {
    let bs = fs.sb.block_size() as u64;
    let byte_in_bgt = gi as u64 * fs.sb.desc_size as u64;
    let bgt_block = fs.sb.first_data_block as u64 + 1 + byte_in_bgt / bs;
    bgt_block * bs + byte_in_bgt % bs
}

/// The descriptor flags of group `gi` as they are ON DISK now.
pub fn on_disk_flags(dev: &Faulty, fs: &Filesystem, gi: usize) -> BgdFlags {
    let mut desc = [0u8; 2];
    dev.read_at(descriptor_offset(fs, gi) + 0x12, &mut desc)
        .expect("read flags");
    BgdFlags::from_bits_truncate(u16::from_le_bytes(desc))
}

/// The group the tested write wakes up: uninit until that write's commit.
pub const TARGET_GROUP: usize = 2;

/// A formatted, journal-less, metadata_csum volume with groups 1 and 2
/// uninit, group 1 then filled by `/spread/fill`, and an empty `/spread/f`
/// and `/spread/g` beside it. The next block `/spread/f` is given therefore
/// comes out of [`TARGET_GROUP`], whose `BLOCK_UNINIT` that write clears.
///
/// Group 1 is woken by the `mkdir` itself (Orlov places the directory there
/// and its block with it), which is why it is filled rather than used: the
/// flag must come down in the write under test, not in the setup.
///
/// The last group stays initialised: the independent checker rejects a
/// final group flagged `BLOCK_UNINIT`.
pub fn prepared() -> Arc<Faulty> {
    let dev = Faulty::new(IMAGE_BYTES);
    mkfs::format_filesystem(dev.as_ref(), Some("DIRECT"), None, IMAGE_BYTES, BLOCK_SIZE)
        .expect("format");
    {
        let fs = Filesystem::mount(dev.clone()).expect("mount to locate groups");
        assert!(fs.journal.is_none(), "the default format has no journal");
        assert_eq!(fs.groups.len(), 4, "the layout this scenario is built on");
        for gi in 1..=TARGET_GROUP {
            mark_group_uninit(&dev, &fs, gi);
        }
    }
    let fs = Filesystem::mount(dev.clone()).expect("mount");
    fs.apply_mkdir("/spread", 0o755).expect("mkdir");
    for name in ["/spread/fill", "/spread/f", "/spread/g"] {
        fs.apply_create(name, 0o644).expect("create");
    }
    drop(fs);
    let fs = Filesystem::mount(dev.clone()).expect("mount");
    let free = fs.groups[1].free_blocks_count as usize;
    fs.apply_pwrite("/spread/fill", 0, &vec![0x5a; free * BLOCK_SIZE as usize])
        .expect("fill");
    drop(fs);
    dev
}

/// What happened when the `fail_at`-th write of `/spread/f`'s first pwrite
/// failed and the same mount was then asked to write `/spread/g`.
pub struct Outcome {
    /// Block numbers the first pwrite asked the device to write, in order.
    pub commit_writes: Vec<u64>,
    pub first: Result<u64>,
    pub second: Result<u64>,
    /// [`TARGET_GROUP`]'s block bitmap block.
    pub bitmap_block: u64,
    /// [`TARGET_GROUP`]'s descriptor flags on disk after the first pwrite.
    pub target_flags: BgdFlags,
    /// [`TARGET_GROUP`]'s block bitmap on disk after the first pwrite.
    pub target_bitmap: Vec<u8>,
    /// The superblock's `s_state` on disk after the mount was dropped.
    pub state_after_drop: u16,
    pub dev: Arc<Faulty>,
}

pub const FIRST: [u8; 4096] = [0xab; 4096];
pub const SECOND: [u8; 4096] = [0xcd; 4096];

/// Run the scenario, failing the `fail_at`-th device write of the first
/// pwrite's commit ([`NEVER`] for a clean run).
pub fn run(fail_at: usize) -> Outcome {
    let dev = prepared();
    let fs = Filesystem::mount(dev.clone()).expect("mount");
    assert!(
        on_disk_flags(&dev, &fs, TARGET_GROUP).contains(BgdFlags::BLOCK_UNINIT),
        "premise: the target group is still BLOCK_UNINIT"
    );
    // The mount's first write marks the volume not clean in a write of its
    // own; take it here so the armed count starts at the pwrite's commit.
    fs.apply_utimens("/spread/g", 0, 0, 0, 0).expect("touch");
    let bitmap_block = fs.groups[TARGET_GROUP].block_bitmap;

    dev.arm(fail_at);
    let first = fs.apply_pwrite("/spread/f", 0, &FIRST);
    let commit_writes = dev.writes().iter().map(|o| o / BLOCK_SIZE as u64).collect();
    dev.arm(NEVER);
    let target_flags = on_disk_flags(&dev, &fs, TARGET_GROUP);
    let target_bitmap = dev.block(bitmap_block);

    let second = fs.apply_pwrite("/spread/g", 0, &SECOND);
    drop(fs);
    let mut state = [0u8; 2];
    dev.read_at(1024 + 0x3A, &mut state).expect("read s_state");
    Outcome {
        commit_writes,
        first,
        second,
        bitmap_block,
        target_flags,
        target_bitmap,
        state_after_drop: u16::from_le_bytes(state),
        dev,
    }
}
