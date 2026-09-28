//! Content writes to the kernel's inline-data files, judged by e2fsprogs
//! (#383, #428).
//!
//! An inline file's `i_block` holds its first 60 bytes. Replacing its
//! content once read those bytes as block pointers and freed the blocks
//! they named, and growing it patched only `i_size` (#383); those writes
//! were then refused. They now go through: in the inode while the result
//! fits, converting the file to an extent-mapped one when it does not. The
//! files here are the kernel's own, in `ext4-inline.img`; `debugfs stat`
//! and `cat` read them back and `e2fsck -fn` judges the volume. The
//! e2fsprogs tools run in the harness VM; a test fails when it cannot reach
//! them.

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use std::sync::Arc;

fn inode_of(fs: &Filesystem, path: &str) -> (u32, fs_ext4::inode::Inode) {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    let ino = fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, path).unwrap();
    (ino, fs.read_inode_verified(ino).unwrap().0)
}

/// What debugfs reads for `path`: its `i_size` from `stat`, and `cat`'s
/// bytes cut to that size. For an inline file `cat` prints the whole
/// inline area, zeros past `i_size` included.
#[track_caller]
fn debugfs_view(image: &str, path: &str) -> Vec<u8> {
    let stat = fs_ext4_test_support::oracle("debugfs")
        .args(["-R", &format!("stat {path}"), image])
        .output();
    let text = String::from_utf8_lossy(&stat.stdout);
    let size: usize = text
        .split_whitespace()
        .skip_while(|w| *w != "Size:")
        .nth(1)
        .and_then(|w| w.parse().ok())
        .unwrap_or_else(|| panic!("no Size: in debugfs stat {path}:\n{text}"));
    let cat = fs_ext4_test_support::oracle("debugfs")
        .args(["-R", &format!("cat {path}"), image])
        .output();
    assert!(cat.status.success(), "debugfs cat {path}");
    let mut bytes = cat.stdout;
    bytes.resize(size, 0);
    bytes
}

#[test]
fn writes_to_the_kernels_inline_files_leave_e2fsck_clean() {
    let src = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), "ext4-inline.img");
    let image =
        fs_ext4_test_support::temp_path!("fs_ext4_inline_writes_{}.img", std::process::id());
    std::fs::copy(&src, &image).unwrap_or_else(|e| panic!("copy {src} -> {image}: {e}"));

    let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
    for path in ["/tiny.txt", "/medium.txt"] {
        assert!(
            inode_of(&fs, path).1.has_inline_data(),
            "{path}: not inline, so this test proves nothing"
        );
    }
    // /tiny.txt stays inline: written, grown into system.data, shrunk.
    let (tiny, _) = inode_of(&fs, "/tiny.txt");
    fs.apply_pwrite("/tiny.txt", 5, b"XY").unwrap();
    fs.apply_truncate_grow(tiny, 90).unwrap();
    fs.apply_pwrite_ino(tiny, 80, b"attr").unwrap();
    fs.apply_truncate_shrink(tiny, 84).unwrap();
    // /medium.txt is converted by a write past what the inode holds.
    fs.apply_pwrite("/medium.txt", 50, &[b'm'; 6000]).unwrap();
    assert!(inode_of(&fs, "/tiny.txt").1.has_inline_data());
    assert!(!inode_of(&fs, "/medium.txt").1.has_inline_data());
    drop(fs);

    let mut tiny_want = b"tiny XYline\n".to_vec();
    tiny_want.resize(80, 0);
    tiny_want.extend_from_slice(b"attr");
    let mut medium_want = vec![b'A'; 50];
    medium_want.extend_from_slice(&[b'm'; 6000]);
    assert_eq!(debugfs_view(&image, "/tiny.txt"), tiny_want, "/tiny.txt");
    assert_eq!(
        debugfs_view(&image, "/medium.txt"),
        medium_want,
        "/medium.txt"
    );
    fs_ext4_test_support::assert_e2fsck_clean(&image, "writes to inline files");

    // A replace converts the inline file; the converted one stays converted
    // when its content shrinks, as the kernel's does.
    let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
    fs.apply_replace_file_content("/tiny.txt", &[b'r'; 7000])
        .unwrap();
    fs.apply_replace_file_content("/medium.txt", b"short")
        .unwrap();
    drop(fs);
    assert_eq!(debugfs_view(&image, "/tiny.txt"), vec![b'r'; 7000]);
    assert_eq!(debugfs_view(&image, "/medium.txt"), b"short");
    fs_ext4_test_support::assert_e2fsck_clean(&image, "replaces of inline files");
    let _ = std::fs::remove_file(&image);
}
