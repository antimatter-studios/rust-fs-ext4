//! `Filesystem::read_link` on ext4-basic.img's /link.txt, a fast symlink
//! (target inline in i_block) created by the kernel as `ln -s test.txt`
//! (test-disks/guest-build-images.sh). #290.

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::error::Error;
use fs_ext4::fs::Filesystem;
use fs_ext4::path;
use std::sync::Arc;

fn mount() -> Filesystem {
    let image = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), "ext4-basic.img");
    let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(&image).expect("open image"));
    Filesystem::mount(dev).expect("mount")
}

fn ino_of(fs: &Filesystem, p: &str) -> u32 {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(inode, _)| inode);
    path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, p).expect("lookup")
}

#[test]
fn read_link_returns_the_fast_symlink_target() {
    let fs = mount();
    let ino = ino_of(&fs, "/link.txt");
    assert_eq!(fs.read_link(ino).expect("read_link"), b"test.txt");
}

#[test]
fn read_link_on_a_regular_file_is_invalid_argument() {
    let fs = mount();
    let ino = ino_of(&fs, "/test.txt");
    assert!(matches!(fs.read_link(ino), Err(Error::InvalidArgument(_))));
}
