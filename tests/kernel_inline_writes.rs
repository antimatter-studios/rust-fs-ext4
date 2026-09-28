//! THE KERNEL READS BACK WHAT THIS CRATE WROTE INTO ITS INLINE-DATA FILES
//! (#428).
//!
//! The volume is made by `mkfs.ext4 -O inline_data` and its files by the
//! kernel, so every inline inode here is laid out the kernel's way, not
//! this crate's. This crate then writes each of them: in place where the
//! result still fits in the inode, converting it to an extent-mapped file
//! where it does not. `e2fsck -fn` judges the volume, and the kernel
//! mounts it and hashes every file, the converted and the still-inline
//! alike.

use fs_ext4::block_io::FileDevice;
use fs_ext4::Filesystem;
use fs_ext4_test_support::{guest_kernel_report, guest_kernel_write, oracle, sha256_hex};
use std::sync::Arc;

fn tiny() -> Vec<u8> {
    b"tiny inline\n".to_vec()
}

fn medium() -> Vec<u8> {
    vec![b'A'; 100]
}

fn spliced(model: &[u8], offset: usize, data: &[u8]) -> Vec<u8> {
    let mut out = model.to_vec();
    if out.len() < offset + data.len() {
        out.resize(offset + data.len(), 0);
    }
    out[offset..offset + data.len()].copy_from_slice(data);
    out
}

fn resized(model: &[u8], len: usize) -> Vec<u8> {
    let mut out = model.to_vec();
    out.resize(len, 0);
    out
}

type Op = fn(&Filesystem, u32) -> fs_ext4::Result<()>;

/// `(file, starts as, write, leaves, still inline)`.
type Case = (&'static str, fn() -> Vec<u8>, Op, Vec<u8>, bool);

fn cases() -> Vec<Case> {
    vec![
        (
            "pwrite-in-place",
            tiny,
            |fs, ino| fs.apply_pwrite_ino(ino, 5, b"XY").map(drop),
            spliced(&tiny(), 5, b"XY"),
            true,
        ),
        (
            "pwrite-into-attr",
            tiny,
            |fs, ino| fs.apply_pwrite_ino(ino, 12, &[b'b'; 50]).map(drop),
            spliced(&tiny(), 12, &[b'b'; 50]),
            true,
        ),
        (
            "pwrite-converts",
            medium,
            |fs, ino| fs.apply_pwrite_ino(ino, 100, &[b'c'; 9000]).map(drop),
            spliced(&medium(), 100, &[b'c'; 9000]),
            false,
        ),
        (
            "pwrite-far",
            tiny,
            |fs, ino| fs.apply_pwrite_ino(ino, 1 << 20, b"z").map(drop),
            spliced(&tiny(), 1 << 20, b"z"),
            false,
        ),
        (
            "replace-small",
            medium,
            |fs, _| {
                fs.apply_replace_file_content("/replace-small", b"hello")
                    .map(drop)
            },
            b"hello".to_vec(),
            true,
        ),
        (
            "replace-large",
            tiny,
            |fs, _| {
                fs.apply_replace_file_content("/replace-large", &[b'e'; 5000])
                    .map(drop)
            },
            vec![b'e'; 5000],
            false,
        ),
        (
            "shrink",
            medium,
            |fs, ino| fs.apply_truncate_ino(ino, 30),
            resized(&medium(), 30),
            true,
        ),
        (
            "grow-in-place",
            tiny,
            |fs, ino| fs.apply_truncate_ino(ino, 80),
            resized(&tiny(), 80),
            true,
        ),
        (
            "grow-converts",
            medium,
            |fs, ino| fs.apply_truncate_ino(ino, 8192),
            resized(&medium(), 8192),
            false,
        ),
    ]
}

#[test]
fn the_kernel_reads_back_inline_files_this_crate_wrote() {
    let image =
        fs_ext4_test_support::temp_path!("fs_ext4_kernel_inline_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(32 * 1024 * 1024))
        .unwrap();
    let made = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", "256"])
        .args(["-O", "inline_data,metadata_csum"])
        .arg(&image)
        .output();
    assert!(
        made.status.success(),
        "mkfs.ext4: {}",
        String::from_utf8_lossy(&made.stderr)
    );

    let cases = cases();
    let mut script = String::from("cd \"$MNT\"\n");
    for (name, start, ..) in &cases {
        let text = if start() == tiny() {
            "printf 'tiny inline\\n'"
        } else {
            "head -c 100 /dev/zero | tr '\\0' A"
        };
        script.push_str(&format!("{text} > {name}\n"));
    }
    script.push_str("sync\n");
    let out = guest_kernel_write(&image, &script);
    assert!(
        out.status.success(),
        "the kernel could not populate the volume:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
    for (name, start, op, want, inline) in &cases {
        let ino = fs.lookup_at(2u32, name.as_bytes()).unwrap();
        let before = fs.stat_ino(ino).unwrap();
        assert!(
            before.has_inline_data() && before.size == start().len() as u64,
            "{name}: the kernel did not make an inline file, so this proves nothing"
        );
        op(&fs, ino).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let after = fs.stat_ino(ino).unwrap();
        assert_eq!(after.has_inline_data(), *inline, "{name}: inline");
        assert_eq!(after.size, want.len() as u64, "{name}: size");
    }
    drop(fs);

    fs_ext4_test_support::assert_e2fsck_clean(&image, "inline files written");
    let report = guest_kernel_report(&image, "inline files written");
    for (name, _, _, want, _) in &cases {
        let key = |kind: &str| (kind.to_string(), name.to_string());
        assert_eq!(
            report.get(&key("size")),
            Some(&want.len().to_string()),
            "{name}: the kernel's size"
        );
        assert_eq!(
            report.get(&key("sha256")),
            Some(&sha256_hex(want)),
            "{name}: the kernel's bytes"
        );
    }
    let _ = std::fs::remove_file(&image);
}
