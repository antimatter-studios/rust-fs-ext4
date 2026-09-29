//! `fs.ext4 write` and `mkdir` on images our own `mkfs.ext4` makes: every
//! file reads back byte for byte, directories list as directories, the
//! volume is left clean (our fsck, and `dirty: false`), and every refusal
//! is a structured error that leaves the image alone.
//!
//! What e2fsprogs makes of the same writes is tests/cli_fs_write_oracle.rs.

mod cli_support;

use cli_support::*;

#[test]
fn every_file_written_reads_back_byte_for_byte() {
    let img = written_image("roundtrip");
    for (path, bytes) in write_cases() {
        let out = ok(tool("fs.ext4").args([&img, "read", path]));
        assert!(out.stdout == bytes, "{path}: read back differs");
        let listed = stdout(&ok(tool("fs.ext4").args([&img, "ls", path])));
        assert_eq!(
            json_field(&listed, "size"),
            bytes.len().to_string(),
            "{path}"
        );
        assert_eq!(json_field(&listed, "type"), "file", "{path}");
    }
}

#[test]
fn write_reports_what_it_did() {
    let img = image_path("report");
    ok(tool("mkfs.ext4").args(["-q", "--text", "--size", "32M", &img]));
    let first = fs_write(&img, "/f", b"hello\n");
    assert!(first.status.success(), "{}", stderr(&first));
    let json = stdout(&first);
    assert_eq!(json_field(&json, "path"), "/f");
    assert_eq!(json_field(&json, "bytes"), "6");
    assert_eq!(json_field(&json, "created"), "true");
    let again = fs_write(&img, "/f", b"hi");
    assert_eq!(json_field(&stdout(&again), "created"), "false");
    assert_eq!(json_field(&stdout(&again), "bytes"), "2");
}

#[test]
fn mkdir_makes_directories_that_list_as_directories() {
    let img = written_image("mkdir");
    let root = stdout(&ok(tool("fs.ext4").args([&img, "ls", "/"])));
    let d = &root[root.find("\"name\": \"d\"").expect("d listed")..];
    assert_eq!(json_field(d, "type"), "dir");
    let inside = stdout(&ok(tool("fs.ext4").args([&img, "ls", "/d"])));
    let e = &inside[inside.find("\"name\": \"e\"").expect("e listed")..];
    assert_eq!(json_field(e, "type"), "dir");
    assert_eq!(json_field(e, "mode"), "0755");
}

#[test]
fn the_volume_is_clean_after_the_writes() {
    let img = written_image("clean");
    let out = tool("fsck.ext4").arg(&img).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let dirty = stdout(&ok(tool("fs.ext4").args([&img, "get", "dirty", "--text"])));
    assert_eq!(dirty, "false\n");
}

#[test]
fn refusals_are_structured_and_leave_the_image_alone() {
    let img = written_image("refusals");
    let before = std::fs::read(&img).unwrap();
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec!["mkdir", "/d"], "already exists"),
        (vec!["mkdir", "/missing/child"], "not found"),
    ];
    for (args, says) in cases {
        let mut full = vec![img.as_str()];
        full.extend(&args);
        let out = tool("fs.ext4").args(&full).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(stderr(&out).contains(says), "{args:?}: {}", stderr(&out));
    }
    for (path, says) in [("/missing/file", "not found"), ("/d", "is a directory")] {
        let out = fs_write(&img, path, b"x");
        assert_eq!(out.status.code(), Some(1), "write {path}");
        assert!(out.stdout.is_empty(), "write {path}");
        assert!(
            stderr(&out).contains(says),
            "write {path}: {}",
            stderr(&out)
        );
    }
    assert!(
        std::fs::read(&img).unwrap() == before,
        "a refused write or mkdir changed the image"
    );
}

#[test]
fn set_label_is_still_not_implemented() {
    let img = image_path("label");
    ok(tool("mkfs.ext4").args(["-q", "--text", "--size", "32M", &img]));
    let out = tool("fs.ext4")
        .args([&img, "set", "label", "X"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(stderr(&out).contains("not implemented"), "{}", stderr(&out));
}
