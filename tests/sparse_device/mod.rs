//! A device that stores only the chunks written to it, so a volume of
//! many block groups costs what its metadata costs. Shared by the mkfs
//! tests that format multi-group volumes in memory.
#![allow(dead_code)]

use fs_ext4::block_io::BlockDevice;
use fs_ext4::error::Result;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const CHUNK: u64 = 4096;

/// A device of `size` bytes that stores only the chunks written to it, so
/// a volume of several groups costs what its metadata costs.
pub struct Sparse {
    size: u64,
    chunks: Mutex<BTreeMap<u64, Vec<u8>>>,
}

impl Sparse {
    pub fn new(size: u64) -> Arc<Self> {
        Arc::new(Self {
            size,
            chunks: Mutex::new(BTreeMap::new()),
        })
    }

    /// One past the last byte anything has been written to.
    pub fn written_end(&self) -> u64 {
        self.chunks
            .lock()
            .unwrap()
            .keys()
            .next_back()
            .map_or(0, |c| (c + 1) * CHUNK)
    }
}

impl BlockDevice for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        assert!(offset + buf.len() as u64 <= self.size, "read past the end");
        let chunks = self.chunks.lock().unwrap();
        for (i, byte) in buf.iter_mut().enumerate() {
            let at = offset + i as u64;
            *byte = chunks
                .get(&(at / CHUNK))
                .map_or(0, |c| c[(at % CHUNK) as usize]);
        }
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.size
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        assert!(offset + buf.len() as u64 <= self.size, "write past the end");
        let mut chunks = self.chunks.lock().unwrap();
        for (i, &byte) in buf.iter().enumerate() {
            let at = offset + i as u64;
            chunks
                .entry(at / CHUNK)
                .or_insert_with(|| vec![0; CHUNK as usize])[(at % CHUNK) as usize] = byte;
        }
        Ok(())
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}
