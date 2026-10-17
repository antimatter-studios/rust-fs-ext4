//! Export bytes written and SHA-256 digested by the Linux kernel in the
//! harness VM. The damaged copy keeps a readable inode but no readable
//! extent tree, so a failed transfer must leave no published destination.

mod cli_support;

use cli_support::*;
use fs_ext4::block_io::FileDevice;
use fs_ext4::Filesystem;
use fs_ext4_test_support::{assert_e2fsck_clean, guest_kernel_write, oracle, sha256_hex};
use std::path::Path;
use std::sync::Arc;

fn kernel_image(tag: &str) -> (String, String) {
    let img = sized_image(tag, 32 << 20);
    let made = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", &img])
        .output();
    assert!(made.status.success(), "mkfs.ext4: {}", stderr(&made));
    let out = guest_kernel_write(
        &img,
        "cd \"$MNT\"\n\
         : > empty\n\
         head -c 3145729 /dev/urandom > big\n\
         mkdir dir\n\
         ln -s big link\n\
         mkfifo pipe\n\
         sha256sum empty big\n\
         sync\n",
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert_e2fsck_clean(&img, "the kernel-written export source");
    (img, stdout(&out))
}

#[test]
fn export_matches_kernel_digests_and_shared_publication_rules() {
    let (img, sums) = kernel_image("export-kernel");
    let before = std::fs::read(&img).unwrap();
    for (name, size) in [("empty", 0), ("big", 3_145_729)] {
        let want = sums
            .lines()
            .find_map(|l| l.strip_suffix(&format!("  {name}")))
            .filter(|h| h.len() == 64)
            .expect("the guest printed the source digest");
        let path = format!("/{name}");
        let dest = image_path(&format!("export-{name}"));
        let report = stdout(&ok(tool("fs.ext4").args([&img, "export", &path, &dest])));
        assert_eq!(json_field(&report, "bytes"), size.to_string());
        assert_eq!(json_field(&report, "sha256"), want);
        assert_eq!(json_field(&report, "replaced"), "false");
        let bytes = std::fs::read(&dest).unwrap();
        assert_eq!(bytes.len(), size);
        assert_eq!(sha256_hex(&bytes), want);

        let raw = ok(tool("fs.ext4").args([&img, "export", &path, "-"]));
        assert_eq!(raw.stdout, bytes);
        assert!(raw.stderr.is_empty());

        std::fs::write(&dest, b"keep me").unwrap();
        let refused = tool("fs.ext4")
            .args([&img, "export", &path, &dest])
            .output()
            .unwrap();
        assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
        assert_eq!(std::fs::read(&dest).unwrap(), b"keep me");
        let replaced = stdout(&ok(tool("fs.ext4").args([
            &img,
            "export",
            "--replace",
            &path,
            &dest,
        ])));
        assert_eq!(json_field(&replaced, "replaced"), "true");
        assert_eq!(sha256_hex(&std::fs::read(&dest).unwrap()), want);
    }

    for path in ["/dir", "/link", "/pipe", "/missing"] {
        let dest = image_path("export-refused");
        let refused = tool("fs.ext4")
            .args([&img, "export", path, &dest])
            .output()
            .unwrap();
        assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
        assert!(refused.stdout.is_empty());
        assert!(!Path::new(&dest).exists());
    }
    let usage = tool("fs.ext4")
        .args([&img, "export", "--replace", "/big", "-"])
        .output()
        .unwrap();
    assert_eq!(usage.status.code(), Some(2), "{}", stderr(&usage));
    assert!(usage.stdout.is_empty());
    let help = stdout(&ok(tool("fs.ext4").args([&img, "export", "--help"])));
    assert!(help.contains("Examples:"), "{help}");
    assert!(help.contains("fs.ext4 disk.img export"), "{help}");

    let embedded = image_path("export-offset");
    let mut bytes = vec![0; 4096];
    bytes.extend_from_slice(&before);
    std::fs::write(&embedded, bytes).unwrap();
    let offset = ok(tool("fs.ext4").args(["--offset", "4096", &embedded, "export", "/big", "-"]));
    let original = ok(tool("fs.ext4").args([&img, "read", "/big"]));
    assert_eq!(offset.stdout, original.stdout);
    assert_eq!(
        std::fs::read(&img).unwrap(),
        before,
        "export wrote the source"
    );
}

#[test]
fn unreadable_export_never_publishes_or_replaces_a_destination() {
    let (img, _) = kernel_image("export-damaged");
    let ino = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&img).unwrap())).unwrap();
        fs.lookup_path_bytes(b"/big").unwrap()
    };
    destroy_extent_header(&img, ino);
    let dest = image_path("export-unreadable");
    let failed = tool("fs.ext4")
        .args([&img, "export", "/big", &dest])
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1), "{}", stderr(&failed));
    assert!(failed.stdout.is_empty());
    assert!(!Path::new(&dest).exists());

    std::fs::write(&dest, b"original destination").unwrap();
    let failed = tool("fs.ext4")
        .args([&img, "export", "--replace", "/big", &dest])
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1), "{}", stderr(&failed));
    assert_eq!(std::fs::read(&dest).unwrap(), b"original destination");
    let dest_path = Path::new(&dest);
    let stem = format!(".{}.", dest_path.file_name().unwrap().to_string_lossy());
    for entry in std::fs::read_dir(dest_path.parent().unwrap()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!name.starts_with(&stem), "staging file remained: {name}");
    }
}
