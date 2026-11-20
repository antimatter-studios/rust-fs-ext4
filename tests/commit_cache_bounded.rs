//! A journaled commit does not pin its blocks in the buffer cache (#328).
//!
//! The journal writer checkpoints every transaction to its final location
//! before `commit` returns, so the device already holds what was committed.
//! Pinning those blocks anyway kept every distinct block a write touched in
//! memory until unmount: the pinned set grew with the work done, and the
//! cache's capacity bounded nothing.
//!
//! Pins are still right where the bytes are not on the device: a read-only
//! mount replays a dirty journal into the cache, and writes nothing (#72,
//! #298). The second test keeps that case pinned, including across
//! `fresh_read`.

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::error::Result;
use fs_ext4::Filesystem;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

#[track_caller]
fn copy_to_tmp(name: &str, tag: &str) -> String {
    let src = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), name);
    let dst =
        fs_ext4_test_support::temp_path!("fs_ext4_commit_cache_{tag}_{}.img", std::process::id());
    std::fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy {src} -> {dst}: {e}"));
    dst
}

fn root_names(fs: &Filesystem) -> Vec<String> {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    let ino = fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, "/").expect("lookup");
    let (inode, _) = fs.read_inode_verified(ino).unwrap();
    let data = fs_ext4::file_io::read_all(fs, &inode).unwrap();
    let bs = fs.sb.block_size() as usize;
    let mut out: Vec<String> = data
        .chunks(bs)
        .flat_map(|b| fs_ext4::dir::parse_block(b, true).unwrap_or_default())
        .map(|e| String::from_utf8_lossy(&e.name).into_owned())
        .collect();
    out.sort();
    out
}

/// N distinct files through one journaled mount leave no more pinned
/// blocks than the cache's capacity. Before the fix the pinned count was
/// every block the writes had touched.
#[test]
fn journaled_writes_do_not_pin_blocks_past_the_cache_capacity() {
    const CAPACITY: usize = 8;
    const FILES: usize = 64;
    let image = copy_to_tmp("ext4-basic.img", "bounded");
    {
        let fs =
            Filesystem::mount_with_cache(Arc::new(FileDevice::open_rw(&image).unwrap()), CAPACITY)
                .expect("mount rw");
        assert!(
            fs.journal.is_some(),
            "the writes must go through the journal"
        );
        for i in 0..FILES {
            let path = format!("/f{i:03}");
            fs.apply_create(&path, 0o644).expect("create");
            fs.apply_replace_file_content(&path, &vec![i as u8; 5000])
                .expect("write");
        }
        let pinned = fs.cache_pinned_blocks();
        assert!(
            pinned <= CAPACITY,
            "{FILES} journaled file writes left {pinned} blocks pinned in a cache of capacity {CAPACITY}"
        );
        // Every commit was checkpointed, so nothing is owed to the device.
        assert_eq!(pinned, 0, "checkpointed blocks are still pinned");
        // Reads served past the pins still see every committed write.
        let names = root_names(&fs);
        for i in 0..FILES {
            assert!(
                names.contains(&format!("f{i:03}")),
                "f{i:03} missing: {names:?}"
            );
        }
    }
    // And the device holds them: a fresh mount reads them back.
    let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).expect("remount");
    let names = root_names(&fs);
    for i in 0..FILES {
        assert!(
            names.contains(&format!("f{i:03}")),
            "f{i:03} lost: {names:?}"
        );
    }
    drop(fs);
    let _ = std::fs::remove_file(&image);
}

/// Drops every write after the dirty journal superblock is flushed, so the
/// last commit is in the log and at no final location.
struct CutAfterDirtyJournal {
    inner: Arc<dyn BlockDevice>,
    armed: AtomicBool,
    flushes: AtomicUsize,
}

impl BlockDevice for CutAfterDirtyJournal {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.armed.load(Ordering::SeqCst) && self.flushes.load(Ordering::SeqCst) >= 2 {
            return Ok(());
        }
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> Result<()> {
        if self.armed.load(Ordering::SeqCst) {
            self.flushes.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.flush()
    }
    fn is_writable(&self) -> bool {
        true
    }
}

/// A read-only mount's replayed blocks exist only in the cache, so they stay
/// pinned, and `fresh_read` refuses rather than release them (#310).
#[test]
fn a_read_only_replay_keeps_its_pins_through_fresh_read() {
    let image = copy_to_tmp("ext4-basic.img", "ro_replay");
    {
        let dev = Arc::new(CutAfterDirtyJournal {
            inner: Arc::new(FileDevice::open_rw(&image).unwrap()),
            armed: AtomicBool::new(false),
            flushes: AtomicUsize::new(0),
        });
        let fs = Filesystem::mount(dev.clone()).expect("mount rw");
        assert!(
            fs.journal.is_some(),
            "the mkdir must go through the journal"
        );
        fs.apply_mkdir("/warmup", 0o755).expect("warm-up mkdir");
        dev.armed.store(true, Ordering::SeqCst);
        fs.apply_mkdir("/committed", 0o755).expect("mkdir");
    }
    let before = fs_ext4_test_support::snapshot_file(&image).unwrap();

    let mut ro = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).expect("mount ro");
    assert!(
        ro.journal.is_none(),
        "a read-only mount has no journal writer"
    );
    let pinned = ro.cache_pinned_blocks();
    assert!(
        pinned > 0,
        "the read-only mount replayed nothing into the cache"
    );
    assert!(
        root_names(&ro).contains(&"committed".to_string()),
        "the replayed view lost the committed mkdir"
    );

    let fresh = ro.fresh_read();
    assert!(
        fresh.is_err(),
        "fresh_read claimed a physical readback the device cannot give"
    );
    assert_eq!(
        ro.cache_pinned_blocks(),
        pinned,
        "fresh_read released blocks that exist only in the cache"
    );
    assert!(
        root_names(&ro).contains(&"committed".to_string()),
        "after fresh_read the committed mkdir is gone"
    );
    drop(ro);
    fs_ext4_test_support::assert_files_equal(&image, before);
    let _ = std::fs::remove_file(&image);
}
