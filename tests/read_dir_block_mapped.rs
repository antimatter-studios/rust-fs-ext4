//! A directory mapped by direct and indirect block pointers -- every
//! directory on ext2 and ext3 -- is listed by `Filesystem::read_dir_ino`,
//! as an extent-mapped one is.
//!
//! The listing refused such a directory ("legacy (non-extent) dirs not yet
//! supported") while path lookup through it worked, so a caller could open
//! `/d/f` but not list `/d`.

use fs_ext4::block_io::BlockDevice;
use fs_ext4::features::FsFlavor;
use fs_ext4::mkfs::format_filesystem_with_flavor;
use fs_ext4::Filesystem;
use std::sync::{Arc, Mutex};

struct Memory(Mutex<Vec<u8>>);

impl BlockDevice for Memory {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_ext4::Result<()> {
        let bytes = self.0.lock().unwrap();
        buf.copy_from_slice(&bytes[offset as usize..offset as usize + buf.len()]);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_ext4::Result<()> {
        let mut bytes = self.0.lock().unwrap();
        bytes[offset as usize..offset as usize + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.0.lock().unwrap().len() as u64
    }
    fn is_writable(&self) -> bool {
        true
    }
}

fn names(flavor: FsFlavor) -> Vec<String> {
    let memory = Arc::new(Memory(Mutex::new(vec![0; 8 << 20])));
    format_filesystem_with_flavor(&*memory, None, Some([5; 16]), 8 << 20, 1024, flavor).unwrap();
    let fs = Filesystem::mount(memory).unwrap();
    let dir = fs.apply_mkdir("/d", 0o755).unwrap();
    for i in 0..3 {
        fs.apply_create(&format!("/d/f{i}"), 0o644).unwrap();
    }
    let mut names: Vec<String> = fs
        .read_dir_ino(dir)
        .unwrap()
        .into_iter()
        .map(|entry| String::from_utf8(entry.name).unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn an_ext2_directory_is_listed() {
    assert_eq!(names(FsFlavor::Ext2), [".", "..", "f0", "f1", "f2"]);
}

#[test]
fn an_ext3_directory_is_listed() {
    assert_eq!(names(FsFlavor::Ext3), [".", "..", "f0", "f1", "f2"]);
}

#[test]
fn an_ext4_directory_is_still_listed() {
    assert_eq!(names(FsFlavor::Ext4), [".", "..", "f0", "f1", "f2"]);
}
