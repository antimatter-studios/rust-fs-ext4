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

/// A fresh `mkfs.ext4 -O inline_data` volume, populated by the kernel with
/// `script` (run in the mount point).
fn volume(tag: &str, script: &str) -> String {
    let image =
        fs_ext4_test_support::temp_path!("fs_ext4_kernel_inline_{tag}_{}.img", std::process::id());
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
    let out = guest_kernel_write(&image, &format!("cd \"$MNT\"\n{script}sync\n"));
    assert!(
        out.status.success(),
        "the kernel could not populate the volume:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    image
}

#[test]
fn the_kernel_reads_back_inline_files_this_crate_wrote() {
    let cases = cases();
    let mut script = String::new();
    for (name, start, ..) in &cases {
        let text = if start() == tiny() {
            "printf 'tiny inline\\n'"
        } else {
            "head -c 100 /dev/zero | tr '\\0' A"
        };
        script.push_str(&format!("{text} > {name}\n"));
    }
    let image = volume("files", &script);

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

/// The kernel's inline directories, one per operation, and the paths the
/// volume holds once this crate has made each.
const DIRS: &str = "\
printf x > x
printf x2 > x2
for d in create mkdir symlink link unlink out in within moved empty convert; do mkdir d-$d; done
for d in unlink out within moved; do printf f > d-$d/f; done
";

#[test]
fn the_kernel_reads_back_inline_directories_this_crate_wrote() {
    let image = volume("dirs", DIRS);
    let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
    let dir = |name: &str| fs.lookup_at(2u32, name.as_bytes()).unwrap();
    for name in ["d-create", "d-mkdir", "d-moved", "d-convert", "d-empty"] {
        assert!(
            fs.stat_ino(dir(name)).unwrap().has_inline_data(),
            "{name}: the kernel did not make an inline directory, so this proves nothing"
        );
    }
    fs.apply_create("/d-create/new", 0o644).unwrap();
    fs.apply_mkdir_at(dir("d-mkdir"), b"sub", 0o755).unwrap();
    fs.apply_symlink("target", "/d-symlink/s").unwrap();
    fs.apply_link("/x", "/d-link/lx").unwrap();
    fs.apply_unlink_at(dir("d-unlink"), b"f").unwrap();
    fs.apply_rename("/d-out/f", "/out-f", false).unwrap();
    fs.apply_rename("/x2", "/d-in/in", false).unwrap();
    let within = dir("d-within");
    fs.apply_rename_at(within, b"f", within, b"g", false)
        .unwrap();
    fs.apply_rename("/d-moved", "/d-mkdir/moved", false)
        .unwrap();
    fs.apply_rmdir("/d-empty").unwrap();
    for i in 0..16 {
        fs.apply_create_at(dir("d-convert"), format!("entry-{i:02}").as_bytes(), 0o644)
            .unwrap();
    }
    assert!(fs.stat_ino(dir("d-create")).unwrap().has_inline_data());
    assert!(!fs.stat_ino(dir("d-convert")).unwrap().has_inline_data());
    drop(fs);

    fs_ext4_test_support::assert_e2fsck_clean(&image, "inline directories written");
    let report = guest_kernel_report(&image, "inline directories written");
    let mut want: Vec<String> = [
        "lost+found",
        "x",
        "out-f",
        "d-create",
        "d-create/new",
        "d-mkdir",
        "d-mkdir/sub",
        "d-mkdir/moved",
        "d-mkdir/moved/f",
        "d-symlink",
        "d-symlink/s",
        "d-link",
        "d-link/lx",
        "d-unlink",
        "d-out",
        "d-in",
        "d-in/in",
        "d-within",
        "d-within/g",
        "d-convert",
    ]
    .map(String::from)
    .to_vec();
    want.extend((0..16).map(|i| format!("d-convert/entry-{i:02}")));
    want.sort();
    let mut got: Vec<String> = report
        .keys()
        .filter(|(kind, _)| kind == "type")
        .map(|(_, path)| path.clone())
        .collect();
    got.sort();
    assert_eq!(got, want, "the paths the kernel lists");
    let get = |kind: &str, path: &str| report.get(&(kind.to_string(), path.to_string()));
    assert_eq!(
        get("target", "d-symlink/s").map(String::as_str),
        Some("target")
    );
    assert_eq!(get("sha256", "d-link/lx"), Some(&sha256_hex(b"x")));
    assert_eq!(get("sha256", "d-in/in"), Some(&sha256_hex(b"x2")));
    assert_eq!(get("sha256", "d-within/g"), Some(&sha256_hex(b"f")));
    let _ = std::fs::remove_file(&image);
}
