//! Truncating something that isn't a regular file is refused (#253).
//!
//! `apply_truncate_grow` and `apply_truncate_shrink` take an inode number
//! and set its size without looking at its type. A directory's size is its
//! blocks, a symlink's is its target's length, and a device node has none,
//! so each came out invalid: e2fsck reported `i_size is 70000, should be
//! 4096`, `Symlink /t (inode #12) is invalid`, and `Special
//! (device/socket/fifo) inode 12 has non-zero size`. Every other operation
//! that changes a file's contents or size already refused them; the kernel
//! answers EISDIR for a directory and EINVAL otherwise. Found by a seeded
//! random sequence of operations checked with e2fsck.
//!
//! Volumes come from `mkfs.ext4`, and `e2fsck -fn` must accept each after
//! the refused truncate. The e2fsprogs tools run in the harness VM; a test fails when it cannot reach them.

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use fs_ext4::Error;
use std::sync::Arc;

#[test]
fn truncate_refuses_directories_symlinks_and_device_nodes() {
    type Make = fn(&Filesystem) -> u32;
    let kinds: [(&str, Make); 4] = [
        ("directory", |fs| fs.apply_mkdir("/t", 0o755).unwrap()),
        ("fast symlink", |fs| {
            fs.apply_symlink("short", "/t").unwrap()
        }),
        ("slow symlink", |fs| {
            fs.apply_symlink(&"u".repeat(300), "/t").unwrap()
        }),
        ("device node", |fs| {
            fs.apply_mknod("/t", 0o020644, 4, 1).unwrap()
        }),
    ];
    for (kind, make) in kinds {
        for grow in [true, false] {
            let image = fs_ext4_test_support::temp_path!(
                "fs_ext4_truncate_type_{}_{grow}_{}.img",
                kind.replace(' ', "_"),
                std::process::id()
            );
            std::fs::File::create(&image)
                .and_then(|f| f.set_len(64 * 1024 * 1024))
                .unwrap();
            let out = fs_ext4_test_support::oracle("mkfs.ext4")
                .args(["-q", "-F", "-b", "4096"])
                .arg(&image)
                .output();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );

            let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
            let ino = make(&fs);
            let got = if grow {
                fs.apply_truncate_grow(ino, 70_000)
            } else {
                fs.apply_truncate_shrink(ino, 1)
            };
            drop(fs);
            let what = if grow { "grow" } else { "shrink" };
            match (kind, &got) {
                ("directory", Err(Error::IsADirectory)) => {}
                ("directory", _) => panic!("[{kind} {what}] expected IsADirectory, got {got:?}"),
                (_, Err(Error::InvalidArgument(_))) => {}
                _ => panic!("[{kind} {what}] expected InvalidArgument, got {got:?}"),
            }
            fs_ext4_test_support::assert_e2fsck_clean(&image, &format!("{kind} {what}"));
            let _ = std::fs::remove_file(&image);
        }
    }
}
