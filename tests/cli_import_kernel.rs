//! Bounded import publication judged by the independent guest kernel and e2fsck.
mod cli_support;
#[path = "../src/cli/ext4/device.rs"]
#[cfg(feature = "cli")]
#[allow(dead_code)]
mod device;
#[path = "../src/cli/ext4/import.rs"]
#[cfg(feature = "cli")]
#[allow(dead_code)]
mod import_adapter;

use cli_support::*;
use fs_ext4_test_support::{assert_e2fsck_clean, guest_kernel_write, oracle, sha256_hex};
use std::io::Write;
use std::process::Stdio;

fn fresh(tag: &str, size: u64) -> String {
    let img = sized_image(tag, size);
    let out = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", &img])
        .output();
    assert!(out.status.success(), "{}", stderr(&out));
    img
}

#[test]
fn imports_match_kernel_digests_and_publish_only_complete_files() {
    let img = fresh("import", 128 << 20);
    for size in [0, 1, 64 << 20] {
        let src = image_path("import-source");
        let bytes = pattern(size, 25);
        std::fs::write(&src, &bytes).unwrap();
        let dest = format!("/f{size}");
        let report = stdout(&ok(
            tool("fs.ext4").args([&img, "import", "--verify", &src, &dest])
        ));
        assert_eq!(json_field(&report, "bytes"), size.to_string());
        assert_eq!(json_field(&report, "sha256"), sha256_hex(&bytes));
        assert_eq!(json_field(&report, "verified"), "true");
        let readback = guest_kernel_write(&img, &format!("sha256sum < \"$MNT{dest}\"\n"));
        assert!(readback.status.success(), "{}", stderr(&readback));
        assert!(stdout(&readback).starts_with(&sha256_hex(&bytes)));
    }
    let src = image_path("replacement");
    std::fs::write(&src, b"replacement").unwrap();
    let before = std::fs::read(&img).unwrap();
    let refused = tool("fs.ext4")
        .args([&img, "import", &src, "/f1"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    assert_eq!(std::fs::read(&img).unwrap(), before);
    let report = stdout(&ok(tool("fs.ext4").args([
        &img,
        "import",
        "--replace",
        "--verify",
        &src,
        "/f1",
    ])));
    assert_eq!(json_field(&report, "replaced"), "true");
    let mut child = tool("fs.ext4")
        .args([&img, "import", "--verify", "-", "/stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"stdin bytes")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let readback = guest_kernel_write(
        &img,
        "cat \"$MNT/f1\"; cat \"$MNT/stdin\"; find \"$MNT\" -name '.dj-import-*.partial'\n",
    );
    assert!(readback.status.success(), "{}", stderr(&readback));
    assert_eq!(stdout(&readback), "replacementstdin bytes");
    let before = std::fs::read(&img).unwrap();
    let alias = image_path("image-alias");
    std::fs::hard_link(&img, &alias).unwrap();
    for source in [&img, &alias] {
        let out = tool("fs.ext4")
            .args([&img, "import", source, "/alias"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert!(stderr(&out).contains("aliases"));
        assert_eq!(std::fs::read(&img).unwrap(), before);
    }
    assert_e2fsck_clean(&img, "complete verified imports");
}

#[test]
fn enospc_import_preserves_old_file_and_removes_staging() {
    let img = fresh("import-full", 16 << 20);
    let out = guest_kernel_write(
        &img,
        "printf old > \"$MNT/old\"\ndd if=/dev/zero of=\"$MNT/filler\" bs=1M count=8 status=none\n",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let src = image_path("too-big");
    let file = std::fs::File::create(&src).unwrap();
    file.set_len(32 << 20).unwrap();
    let out = tool("fs.ext4")
        .args([&img, "import", "--replace", &src, "/old"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    let seen = guest_kernel_write(
        &img,
        "cat \"$MNT/old\"; find \"$MNT\" -name '.dj-import-*.partial'\n",
    );
    assert!(seen.status.success(), "{}", stderr(&seen));
    assert_eq!(stdout(&seen), "old");
    assert_e2fsck_clean(&img, "failed import cleaned staging");
}

#[test]
#[cfg(feature = "cli")]
fn killed_after_persistence_keeps_old_name_and_reports_complete_staging() {
    const CHILD: &str = "EXT4_IMPORT_CRASH_IMAGE";
    if let Some(img) = std::env::var_os(CHILD) {
        let source = std::env::var_os("EXT4_IMPORT_CRASH_SOURCE").unwrap();
        let marker = std::env::var_os("EXT4_IMPORT_CRASH_MARKER").unwrap();
        let mut staged = import_adapter::Stage::create(&img, 0, b"/old", true).unwrap();
        let mut source = import_adapter::LocalSource::open(&source).unwrap();
        staged.copy(&mut source).unwrap();
        std::fs::write(marker, b"persisted").unwrap();
        loop {
            std::thread::park();
        }
    }
    let img = fresh("import-crash", 32 << 20);
    let old = guest_kernel_write(&img, "printf old > \"$MNT/old\"\n");
    assert!(old.status.success(), "{}", stderr(&old));
    let src = image_path("crash-source");
    std::fs::write(&src, b"complete staging").unwrap();
    let marker = image_path("persisted-marker");
    let this_binary = std::env::current_exe().unwrap();
    let mut child = std::process::Command::new(this_binary)
        .args([
            "--exact",
            "killed_after_persistence_keeps_old_name_and_reports_complete_staging",
        ])
        .env(CHILD, &img)
        .env("EXT4_IMPORT_CRASH_SOURCE", &src)
        .env("EXT4_IMPORT_CRASH_MARKER", &marker)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !std::path::Path::new(&marker).exists() && std::time::Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("crash child exited: {status}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        std::path::Path::new(&marker).exists(),
        "child did not persist staging"
    );
    let seen = guest_kernel_write(&img, "cat \"$MNT/old\"; for f in \"$MNT\"/.dj-import-*.partial; do [ -f \"$f\" ] || exit 1; cat \"$f\"; done\n");
    assert!(seen.status.success(), "{}", stderr(&seen));
    assert_eq!(stdout(&seen), "oldcomplete staging");
    let listing = stdout(&ok(tool("fs.ext4").args([&img, "ls", "/"])));
    assert!(
        listing.contains(".dj-import-"),
        "staging must be discoverable: {listing}"
    );
    assert_e2fsck_clean(&img, "kill between persist and publish");
}
