//! The browser build, RUN rather than merely compiled.
//!
//! `wasm32-unknown-unknown` is a target this crate supports: `Cargo.toml`
//! pulls in `js-sys` for it and `src/runtime.rs` reads wall time through
//! JavaScript there. A build-only check proves nothing about it, because
//! `std` on that target COMPILES `SystemTime::now`, `Instant::now` and
//! `std::process::id` and panics when they are called. So these tests run
//! under `wasm-bindgen-test` in Node (`chore test:wasm`), over an in-memory
//! `BlockDevice`, the only kind of device a browser host has.
//!
//! On every other target this file compiles to nothing.
#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

use fs_ext4::block_io::BlockDevice;
use fs_ext4::error::Result;
use fs_ext4::file_io;
use fs_ext4::fs::Filesystem;
use std::sync::{Arc, Mutex};
use wasm_bindgen_test::wasm_bindgen_test;

const SIZE: u64 = 16 * 1024 * 1024;
const BLOCK_SIZE: u32 = 4096;
const UUID: [u8; 16] = [
    0x6a, 0x1e, 0x77, 0x02, 0x3c, 0x9d, 0x4b, 0x51, 0x8e, 0x20, 0x11, 0xf4, 0x5a, 0x09, 0xc3, 0x7d,
];

/// A volume held in memory, as a browser host holds one.
struct MemDev {
    bytes: Mutex<Vec<u8>>,
}

impl MemDev {
    fn new(size: u64) -> Arc<Self> {
        Arc::new(Self {
            bytes: Mutex::new(vec![0u8; size as usize]),
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
        self.bytes.lock().unwrap().len() as u64
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

/// A volume formatted with a caller-supplied UUID, so no randomness is
/// needed to make it.
fn formatted() -> Arc<MemDev> {
    let dev = MemDev::new(SIZE);
    fs_ext4::mkfs::format_filesystem(dev.as_ref(), Some("wasm"), Some(UUID), SIZE, BLOCK_SIZE)
        .expect("format_filesystem with a caller-supplied UUID");
    dev
}

fn mount(dev: &Arc<MemDev>) -> Filesystem {
    let dev: Arc<dyn BlockDevice> = dev.clone();
    Filesystem::mount(dev).expect("mount")
}

fn ino_of(fs: &Filesystem, path: &str) -> u32 {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, path).expect("lookup")
}

#[wasm_bindgen_test]
fn a_formatted_volume_mounts_with_an_empty_root() {
    let dev = formatted();
    let fs = mount(&dev);
    assert_eq!(fs.sb.volume_name, "wasm");
    assert_eq!(fs.sb.uuid, UUID);
    assert_eq!(fs.sb.block_size(), BLOCK_SIZE);
    assert!(fs.sb.is_clean(), "a fresh volume is marked clean");
    let (root, _) = fs.read_inode_verified(2).expect("root inode verifies");
    assert!(root.is_dir(), "the root is a directory");
    assert_eq!(root.links_count, 2, "`.` and `..`");
    fs.finish().expect("unmount");
}

#[wasm_bindgen_test]
fn a_file_created_and_written_reads_back_after_unmount() {
    let dev = formatted();
    let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8 + 1).collect();
    {
        let fs = mount(&dev);
        // The default runtime: wall time and inode generations come from
        // src/runtime.rs's browser path.
        let ino = fs.apply_create("/hello.bin", 0o644).expect("create");
        let (inode, _) = fs.read_inode_verified(ino).expect("new inode verifies");
        assert!(inode.mtime > 0, "the browser clock stamped the new inode");
        let size = fs
            .apply_pwrite("/hello.bin", 0, &data)
            .expect("write the file");
        assert_eq!(size, data.len() as u64);
        fs.finish().expect("unmount");
    }

    let fs = mount(&dev);
    let (inode, _) = fs
        .read_inode_verified(ino_of(&fs, "/hello.bin"))
        .expect("the file is there after a remount");
    assert_eq!(inode.size, data.len() as u64);
    let got = file_io::read_all(&fs, &inode).expect("read the file back");
    assert!(got == data, "the file does not read back as written");
    fs.finish().expect("unmount");
}

/// A browser host formats without a UUID of its own, so mkfs draws one.
/// It must be a real RFC 4122 version-4 UUID, and two volumes must not
/// share one (#294).
#[wasm_bindgen_test]
fn format_without_a_uuid_draws_a_random_v4_uuid() {
    let uuid_of_a_fresh_volume = || {
        let dev = MemDev::new(SIZE);
        fs_ext4::mkfs::format_filesystem(dev.as_ref(), None, None, SIZE, BLOCK_SIZE)
            .expect("format_filesystem with no UUID");
        let fs = mount(&dev);
        let uuid = fs.sb.uuid;
        fs.finish().expect("unmount");
        uuid
    };
    let a = uuid_of_a_fresh_volume();
    let b = uuid_of_a_fresh_volume();
    for uuid in [a, b] {
        assert_eq!(uuid[6] >> 4, 4, "version nibble of {uuid:02x?}");
        assert_eq!(uuid[8] >> 6, 0b10, "variant bits of {uuid:02x?}");
    }
    assert_ne!(a, b, "two formats drew the same UUID");
}
