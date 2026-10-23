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
    // The volume's state refuses these: exit 4, `refused`, in the CLI
    // contract (EXIT_REFUSED), with the JSON error on stderr.
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec!["mkdir", "/d"], "already exists"),
        (vec!["mkdir", "/missing/child"], "not found"),
        (vec!["rmdir", "/d"], "directory not empty"),
    ];
    for (args, says) in cases {
        let mut full = vec![img.as_str()];
        full.extend(&args);
        let out = tool("fs.ext4").args(&full).output().unwrap();
        let err = stderr(&out);
        assert_eq!(out.status.code(), Some(4), "{args:?}: {err}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(err.contains("\"code\": 4"), "{args:?}: {err}");
        assert!(err.contains(says), "{args:?}: {err}");
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
        "a refused write, mkdir or rmdir changed the image"
    );
}

#[test]
fn a_write_that_does_not_fit_leaves_no_file_behind() {
    let img = image_path("nospace");
    ok(tool("mkfs.ext4").args(["-q", "--text", "--size", "8M", &img]));
    let out = fs_write(&img, "/huge", &pattern(16 << 20, 9));
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    let listed = tool("fs.ext4")
        .args([&img, "ls", "/huge"])
        .output()
        .unwrap();
    assert_eq!(
        listed.status.code(),
        Some(1),
        "a failed write left /huge behind: {}",
        stdout(&listed)
    );
    let dirty = stdout(&ok(tool("fs.ext4").args([&img, "get", "dirty", "--text"])));
    assert_eq!(dirty, "false\n");
}

#[test]
fn set_label_rewrites_the_label_and_leaves_the_volume_clean() {
    let img = image_path("label");
    ok(tool("mkfs.ext4").args(["-q", "--text", "--size", "32M", "--label", "BEFORE", &img]));
    let set = stdout(&ok(tool("fs.ext4").args([&img, "set", "label", "AFTER"])));
    assert_eq!(json_field(&set, "label"), "AFTER");
    let label = stdout(&ok(tool("fs.ext4").args([&img, "get", "label", "--text"])));
    assert_eq!(label, "AFTER\n");
    let out = tool("fsck.ext4").arg(&img).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let dirty = stdout(&ok(tool("fs.ext4").args([&img, "get", "dirty", "--text"])));
    assert_eq!(dirty, "false\n");

    ok(tool("fs.ext4").args([&img, "set", "label", ""]));
    let label = stdout(&ok(tool("fs.ext4").args([&img, "get", "label"])));
    assert_eq!(
        label.trim(),
        "{\n  \"label\": null\n}",
        "an empty value clears the label"
    );

    let before = std::fs::read(&img).unwrap();
    let out = tool("fs.ext4")
        .args([&img, "set", "label", "seventeen-bytes!!"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    assert!(stderr(&out).contains("16 bytes"), "{}", stderr(&out));
    assert!(
        std::fs::read(&img).unwrap() == before,
        "a refused label changed the image"
    );
}
