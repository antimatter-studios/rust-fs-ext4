//! THE KERNEL READS BACK WHAT THIS DRIVER WROTE.
//!
//! Every other oracle in this suite is e2fsprogs: a second reader of the
//! same format, from the same project. This one is Linux. The image is
//! built by `mke2fs`, populated ONLY by this crate — directories, files,
//! a multi-megabyte file written in unaligned pieces, a symlink,
//! extended attributes, an ACL, a rename, an unlink, a truncate — and
//! then loop-mounted read-only by the real in-kernel ext4 driver inside
//! the harness VM, which walks it and reports what it sees.
//!
//! What that catches and `e2fsck -fn` does not: a directory entry with a
//! wrong record length that still checksums, an extent the kernel maps
//! shorter than we wrote it, an xattr in a place the kernel does not
//! look, a symlink stored inline when it should be in a block. A
//! consistent filesystem and a filesystem that reads back correctly are
//! different claims.
//!
//! ONE MOUNT PER TEST. The guest script does the whole comparison and
//! prints it; the host compares that against what it wrote.
//!
//! The kernel is only ever asked in the guest (the `guest_kernel_*`
//! helpers in the test support crate): a host mount would need root, and
//! on macOS there is no ext4 at all.

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use fs_ext4_test_support::{guest_kernel_report, oracle, sha256_hex, temp_path};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A fresh `mkfs.ext4` volume, made by the tool in the guest.
fn volume(tag: &str, features: &[&str]) -> String {
    let image = temp_path!("fs_ext4_kernel_{tag}_{}.img", std::process::id());
    let _ = std::fs::remove_file(&image);
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(96 * 1024 * 1024))
        .unwrap_or_else(|e| panic!("size {image}: {e}"));
    let out = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096"])
        .args(features)
        .arg(&image)
        .output();
    assert!(
        out.status.success(),
        "mkfs.ext4 {image}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    image
}

/// The bytes of the multi-megabyte file: cheap to generate, and every
/// 32-bit window is different, so a misplaced block is visible.
fn payload(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state = 0x1234_5678_9abc_def0u64;
    while out.len() < len {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn resolve(fs: &Filesystem, path: &str) -> u32 {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, path).expect("resolve")
}

/// A POSIX ACL as ext4 stores it: version 2, then one entry per
/// `getfacl` line.
///
/// TWO THINGS THE KERNEL REFUSES IF THEY ARE WRONG, AND `e2fsck` DOES
/// NOT LOOK AT. The version is ext4's own (1), not the userspace xattr
/// format's (2). And AN ENTRY WITHOUT AN ID IS FOUR BYTES, NOT EIGHT. `ACL_USER_OBJ`,
/// `ACL_GROUP_OBJ`, `ACL_MASK` and `ACL_OTHER` name nobody, so ext4
/// stores them as `ext4_acl_entry_short` — tag and permissions and
/// nothing else. Writing the eight-byte form for them produces a blob
/// the kernel rejects outright (`getfacl` answers "Invalid argument"),
/// which is exactly what this oracle is for.
fn acl_blob(entries: &[(u16, u16, Option<u32>)]) -> Vec<u8> {
    let mut out = fs_ext4::acl::EXT4_ACL_VERSION.to_le_bytes().to_vec();
    for (tag, perm, id) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&perm.to_le_bytes());
        if let Some(id) = id {
            out.extend_from_slice(&id.to_le_bytes());
        }
    }
    out
}

/// The whole tree, written by this crate's Rust API and read back by the
/// kernel. What it writes is listed in the module comment; what the
/// kernel must see is listed here.
struct Written {
    image: String,
    big: Vec<u8>,
    small: Vec<u8>,
    renamed: Vec<u8>,
    truncated: Vec<u8>,
}

fn write_everything(tag: &str) -> Written {
    let image = volume(tag, &[]);
    let big = payload(5 * 1024 * 1024 + 777);
    let small = b"a small file, written whole\n".to_vec();
    let renamed = b"renamed content\n".to_vec();
    let truncated = payload(200_000)[..4097].to_vec();

    let fs =
        Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).expect("mount to write");
    fs.apply_mkdir("/dir", 0o755).expect("mkdir /dir");
    fs.apply_mkdir("/dir/nested", 0o700).expect("mkdir nested");
    fs.apply_mkdir("/acl_dir", 0o750).expect("mkdir acl_dir");

    fs.apply_create("/dir/small.txt", 0o644).expect("create");
    fs.apply_replace_file_content("/dir/small.txt", &small)
        .expect("write small");

    // The multi-megabyte file, in pieces that straddle block and extent
    // boundaries rather than filling them.
    fs.apply_create("/dir/big.bin", 0o600).expect("create big");
    let mut at = 0usize;
    for len in [3usize, 4093, 1_048_577, 7, 4096 * 300 + 11] {
        let end = (at + len).min(big.len());
        fs.apply_pwrite("/dir/big.bin", at as u64, &big[at..end])
            .expect("pwrite chunk");
        at = end;
    }
    if at < big.len() {
        fs.apply_pwrite("/dir/big.bin", at as u64, &big[at..])
            .expect("pwrite tail");
    }

    fs.apply_symlink("dir/big.bin", "/link-to-big")
        .expect("symlink");
    fs.apply_symlink(LONG_TARGET, "/long-link")
        .expect("long symlink");

    fs.apply_setxattr("/dir/small.txt", "user.colour", b"amber")
        .expect("setxattr");
    fs.apply_setxattr("/dir/small.txt", "user.note", b"written-by-rust-fs-ext4")
        .expect("setxattr");

    fs.apply_setxattr(
        "/acl_dir",
        "system.posix_acl_access",
        &acl_blob(&[
            (0x0001, 0x0007, None),       // user_obj  rwx
            (0x0002, 0x0006, Some(1000)), // user:1000 rw-
            (0x0004, 0x0005, None),       // group_obj r-x
            (0x0010, 0x0007, None),       // mask      rwx
            (0x0020, 0x0004, None),       // other     r--
        ]),
    )
    .expect("set acl");

    // A rename, an unlink and a truncate: operations whose half-finished
    // state a consistency check cannot see but a reader trips over.
    fs.apply_create("/dir/to-rename", 0o644).expect("create");
    fs.apply_replace_file_content("/dir/to-rename", &renamed)
        .expect("write");
    fs.apply_rename("/dir/to-rename", "/dir/nested/renamed", false)
        .expect("rename");

    fs.apply_create("/dir/to-unlink", 0o644).expect("create");
    fs.apply_replace_file_content("/dir/to-unlink", b"gone")
        .expect("write");
    fs.apply_unlink("/dir/to-unlink").expect("unlink");

    fs.apply_create("/dir/truncated", 0o644).expect("create");
    fs.apply_replace_file_content("/dir/truncated", &payload(200_000))
        .expect("write");
    let ino = resolve(&fs, "/dir/truncated");
    fs.apply_truncate_shrink(ino, 4097).expect("truncate");
    drop(fs);

    Written {
        image,
        big,
        small,
        renamed,
        truncated,
    }
}

const LONG_TARGET: &str =
    "a/very/long/target/that/cannot/live/inside/the/inode/because/it/is/far/past/sixty/bytes";

/// Check one kernel report against what was written.
fn check(report: &BTreeMap<(String, String), String>, written: &Written) -> Vec<String> {
    let mut wrong = Vec::new();
    let mut want = |kind: &str, path: &str, value: String| {
        let key = (kind.to_string(), path.to_string());
        match report.get(&key) {
            Some(got) if *got == value => {}
            Some(got) => wrong.push(format!(
                "{kind} of {path}: kernel says {got:?}, wrote {value:?}"
            )),
            None => wrong.push(format!("{kind} of {path}: the kernel did not report it")),
        }
    };

    want("type", "dir", "directory".into());
    want("mode", "dir", "755".into());
    want("type", "dir/nested", "directory".into());
    want("mode", "dir/nested", "700".into());
    want("type", "acl_dir", "directory".into());

    want("type", "dir/small.txt", "regular-file".into());
    want("mode", "dir/small.txt", "644".into());
    want("size", "dir/small.txt", written.small.len().to_string());
    want("sha256", "dir/small.txt", sha256_hex(&written.small));
    want(
        "xattrs",
        "dir/small.txt",
        "user.colour=amber,user.note=written-by-rust-fs-ext4".into(),
    );

    want("type", "dir/big.bin", "regular-file".into());
    want("mode", "dir/big.bin", "600".into());
    want("size", "dir/big.bin", written.big.len().to_string());
    want("sha256", "dir/big.bin", sha256_hex(&written.big));

    want("type", "link-to-big", "symbolic-link".into());
    want("target", "link-to-big", "dir/big.bin".into());
    want("type", "long-link", "symbolic-link".into());
    want("target", "long-link", LONG_TARGET.into());

    want("type", "dir/nested/renamed", "regular-file".into());
    want("sha256", "dir/nested/renamed", sha256_hex(&written.renamed));

    want("size", "dir/truncated", "4097".into());
    want("sha256", "dir/truncated", sha256_hex(&written.truncated));

    for gone in ["dir/to-unlink", "dir/to-rename"] {
        if report.contains_key(&("type".to_string(), gone.to_string())) {
            wrong.push(format!("{gone} is still in the tree the kernel mounted"));
        }
    }

    let acl = report
        .get(&("acl".to_string(), "acl_dir".to_string()))
        .cloned()
        .unwrap_or_default();
    for entry in [
        "user::rwx",
        "user:1000:rw-",
        "group::r-x",
        "mask::rwx",
        "other::r--",
    ] {
        if !acl.contains(entry) {
            wrong.push(format!("the ACL the kernel read has no {entry}: {acl:?}"));
        }
    }
    wrong
}

#[test]
fn the_kernel_reads_back_what_the_rust_api_wrote() {
    let written = write_everything("rust_api");
    let report = guest_kernel_report(&written.image, "rust api");
    let wrong = check(&report, &written);
    assert!(
        wrong.is_empty(),
        "the kernel read back something else:\n{}",
        wrong.join("\n")
    );
    // And the volume it mounted is one e2fsck also accepts: consistency
    // and correct content are different claims, and this suite makes
    // both.
    fs_ext4_test_support::assert_e2fsck_clean(&written.image, "rust api");
    let _ = std::fs::remove_file(&written.image);
}

/// THE NEGATIVE CASE: the comparison above has to be able to fail.
///
/// One byte of the big file is flipped on disk after it was written.
/// `e2fsck -fn` still calls the volume clean — file data is not
/// checksummed — and the kernel readback catches it, which is the whole
/// reason this suite has a kernel oracle at all.
#[test]
fn a_flipped_data_byte_fails_the_comparison_and_not_e2fsck() {
    let written = write_everything("corrupt");

    // Where that file's data actually is, asked of the driver: a byte
    // picked at random would probably land in free space.
    let offset = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&written.image).unwrap()))
            .expect("mount to locate");
        let ino = resolve(&fs, "/dir/big.bin");
        let (inode, _) = fs.read_inode_verified(ino).expect("read inode");
        let extent =
            fs_ext4::extent::lookup(&inode.block, fs.dev.as_ref(), fs.sb.block_size(), 300)
                .expect("map block")
                .expect("logical block 300 is mapped");
        extent.map(300) * u64::from(fs.sb.block_size()) + 17
    };
    let mut bytes = std::fs::read(&written.image).expect("read image");
    bytes[offset as usize] ^= 0xff;
    std::fs::write(&written.image, &bytes).expect("write image");

    // e2fsck is happy: the bytes it checks are all still right.
    fs_ext4_test_support::assert_e2fsck_clean(&written.image, "corrupt");

    // The kernel readback is not.
    let report = guest_kernel_report(&written.image, "corrupt");
    let wrong = check(&report, &written);
    assert!(
        wrong.iter().any(|w| w.starts_with("sha256 of dir/big.bin")),
        "a flipped data byte did not fail the comparison, so the comparison proves \
         nothing. It reported:\n{}",
        wrong.join("\n")
    );
    assert_eq!(
        wrong.len(),
        1,
        "only the corrupted file should differ:\n{}",
        wrong.join("\n")
    );
    let _ = std::fs::remove_file(&written.image);
}

/// THE OTHER DIRECTION: the kernel writes, this driver reads.
///
/// The fixtures are kernel-made too, but they are made once, by a
/// script, into an image nothing of ours ever touched. Here the image is
/// ours — `mkfs` plus a tree this crate wrote — the kernel adds to it
/// through a read-write mount, and the driver has to see exactly what it
/// added. It is read back through the C ABI, which is the interface a
/// consumer actually uses.
#[test]
fn this_driver_reads_back_what_the_kernel_wrote() {
    use fs_ext4::capi::*;
    use std::ffi::CString;

    let image = volume("kernel_writes", &[]);
    let content = payload(300_001);
    // The bytes travel as a FILE the guest reads from the repository it
    // has mounted, not as an argument: a command line is a few hundred
    // kilobytes short of holding them.
    let source = temp_path!("fs_ext4_kernel_source_{}.bin", std::process::id());
    std::fs::write(&source, &content).expect("stage the payload");

    let out = fs_ext4_test_support::guest_kernel_write(
        &image,
        &format!(
            r#"
mkdir -p "$MNT/from-kernel/deeper"
cp '{source}' "$MNT/from-kernel/deeper/written.bin"
ln -s ../deeper/written.bin "$MNT/from-kernel/link"
setfattr -n user.written-by -v kernel "$MNT/from-kernel/deeper/written.bin"
chmod 640 "$MNT/from-kernel/deeper/written.bin"
sync
"#
        ),
    );
    assert!(
        out.status.success(),
        "the kernel could not write into the image:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let image_c = CString::new(image.as_str()).unwrap();
    let path_c = CString::new("/from-kernel/deeper/written.bin").unwrap();
    unsafe {
        let fs = fs_ext4_mount(image_c.as_ptr());
        assert!(!fs.is_null(), "mount the image the kernel wrote into");

        let mut attr = std::mem::zeroed::<fs_ext4_attr_t>();
        assert_eq!(
            fs_ext4_stat(fs, path_c.as_ptr(), &mut attr),
            0,
            "stat the file the kernel created"
        );
        assert_eq!(attr.size, content.len() as u64, "size");
        assert_eq!(attr.mode & 0o777, 0o640, "mode");

        let mut buf = vec![0u8; content.len()];
        let read = fs_ext4_read_file(
            fs,
            path_c.as_ptr(),
            buf.as_mut_ptr() as *mut std::os::raw::c_void,
            0,
            content.len() as u64,
        );
        assert_eq!(read, content.len() as i64, "read the whole file");
        assert_eq!(
            sha256_hex(&buf),
            sha256_hex(&content),
            "the driver read back different bytes than the kernel wrote"
        );

        let link_c = CString::new("/from-kernel/link").unwrap();
        let mut target = vec![0 as std::os::raw::c_char; 256];
        assert_eq!(
            fs_ext4_readlink(fs, link_c.as_ptr(), target.as_mut_ptr(), target.len()),
            "../deeper/written.bin".len() as i32,
            "readlink the symlink the kernel created"
        );
        let target = std::ffi::CStr::from_ptr(target.as_ptr())
            .to_string_lossy()
            .into_owned();
        assert_eq!(target, "../deeper/written.bin");

        let name_c = CString::new("user.written-by").unwrap();
        let mut value = vec![0u8; 64];
        let len = fs_ext4_getxattr(
            fs,
            path_c.as_ptr(),
            name_c.as_ptr(),
            value.as_mut_ptr() as *mut std::os::raw::c_void,
            value.len(),
        );
        assert!(len > 0, "the xattr the kernel set is readable");
        assert_eq!(&value[..len as usize], b"kernel");

        fs_ext4_umount(fs);
    }

    fs_ext4_test_support::assert_e2fsck_clean(&image, "kernel writes");
    let _ = std::fs::remove_file(&image);
}
