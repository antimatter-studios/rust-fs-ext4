//! Proves `Filesystem::mount` wraps the underlying device in
//! `CachedDevice` and that repeated reads of the same path don't
//! cause repeated inner-device reads.
//!
//! Background: the cache used to be opt-in (callers had to wrap the
//! device themselves). It's now automatic in `mount_inner` because
//! the cache holds journaled-but-not-yet-checkpointed bytes that
//! allocators must see before the journal is checkpointed back to
//! the data area on disk. This test pins the read-side behaviour:
//! after the first workload pass, the cache must absorb subsequent
//! lookups so the inner device stays mostly idle.

use fs_ext4::block_io::BlockDevice;
use fs_ext4::error::Result;
use fs_ext4::Filesystem;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct CountingFile {
    bytes: Mutex<Vec<u8>>,
    reads: AtomicU64,
}

impl CountingFile {
    fn from_file(path: &str) -> Arc<Self> {
        let bytes = fs_ext4_test_support::read_small_file(path).expect("read image");
        Arc::new(Self {
            bytes: Mutex::new(bytes),
            reads: AtomicU64::new(0),
        })
    }

    fn reads(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
    }
}

impl BlockDevice for CountingFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let b = self.bytes.lock().unwrap();
        let off = offset as usize;
        if off + buf.len() > b.len() {
            return Err(fs_ext4::error::Error::OutOfBounds);
        }
        buf.copy_from_slice(&b[off..off + buf.len()]);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.bytes.lock().unwrap().len() as u64
    }
    fn is_writable(&self) -> bool {
        false
    }
}

#[track_caller]
fn image_path(name: &str) -> String {
    fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), name)
}

fn workload(fs: &Filesystem) {
    // Lookup the same path 10 times — extent reads should hit cache.
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    for _ in 0..10 {
        let _ = fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, "/test.txt");
    }
}

#[test]
fn repeated_lookups_hit_cache_not_inner_device() {
    let path = image_path("ext4-basic.img");

    let inner = CountingFile::from_file(&path);
    let fs = Filesystem::mount(inner.clone()).expect("mount");

    // First pass populates the cache.
    workload(&fs);
    let after_first = inner.reads();

    // Second pass over the same path should pull almost everything
    // from the cache — the inner device should see <= a small handful
    // of additional reads (bookkeeping at most). If subsequent lookups
    // re-read the same metadata blocks, the cache isn't doing its job.
    workload(&fs);
    let after_second = inner.reads();
    let delta = after_second - after_first;

    println!(
        "repeated_lookups: first-pass reads={}, second-pass delta={}",
        after_first, delta
    );

    assert!(
        delta <= 4,
        "second pass over the same path should be served from cache; \
         got {delta} new inner reads (cache likely not wired up at mount time)"
    );
}
