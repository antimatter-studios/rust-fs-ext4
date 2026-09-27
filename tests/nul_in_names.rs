//! A name holding a NUL byte is refused by every operation that files one.
//!
//! A directory entry's name is counted bytes, not a C string, so nothing in
//! the on-disk format stops a NUL. The kernel never writes one, because a
//! name reaches it as a C string, and `e2fsck` reports one as an illegal
//! character in the name. This crate takes a Rust `&str`, which can hold
//! NUL, and filed it as given: create, mkdir, mknod, symlink, link and
//! rename all wrote the entry.
//!
//! Volumes come from `mkfs.ext4`, and `e2fsck -fn` must accept each one after
//! the refused operation. The e2fsprogs tools run in the harness VM; a test fails when it cannot reach them.

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use std::sync::Arc;

fn mkfs(tag: &str) -> String {
    let mkfs = "mkfs.ext4";
    let path = fs_ext4_test_support::temp_path!("fs_ext4_nul_{tag}_{}.img", std::process::id());
    std::fs::File::create(&path)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .unwrap();
    let out = fs_ext4_test_support::oracle(mkfs)
        .args(["-q", "-F", "-b", "4096"])
        .arg(&path)
        .output();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

#[test]
fn a_name_holding_a_nul_byte_is_refused() {
    type Op = fn(&Filesystem) -> Result<(), fs_ext4::Error>;
    let ops: [(&str, Op); 7] = [
        ("create", |fs| fs.apply_create("/a\0b", 0o644).map(|_| ())),
        ("mkdir", |fs| fs.apply_mkdir("/a\0b", 0o755).map(|_| ())),
        ("mknod", |fs| {
            fs.apply_mknod("/a\0b", 0o010644, 0, 0).map(|_| ())
        }),
        ("symlink", |fs| {
            fs.apply_symlink("target", "/a\0b").map(|_| ())
        }),
        ("link", |fs| fs.apply_link("/f", "/a\0b")),
        ("rename", |fs| fs.apply_rename("/f", "/a\0b", false)),
        ("rename-self", |fs| fs.apply_rename("/a\0b", "/a\0b", false)),
    ];
    for (tag, op) in ops {
        let path = mkfs(tag);
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&path).unwrap())).unwrap();
        fs.apply_create("/f", 0o644).unwrap();
        let got = op(&fs);
        drop(fs);
        assert!(
            matches!(
                got,
                Err(fs_ext4::Error::InvalidArgument(
                    "a name cannot contain a NUL byte"
                ))
            ),
            "[{tag}] a name holding a NUL byte was not refused as one: {got:?}"
        );
        fs_ext4_test_support::assert_e2fsck_clean(&path, tag);
        let _ = std::fs::remove_file(&path);
    }
}
