//! `fs.ext4`'s read verbs against the kernel-made fixtures: `ls`, `read`,
//! `get`/`info`, `--offset` into a whole-disk image, and the verbs that
//! answer `not implemented`.
//!
//! The fixtures were written by the Linux kernel (test-disks/
//! guest-build-images.sh), so the names, types and bytes these tests
//! expect are what the kernel put there, not what this crate believes.

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::fixture;

fn basic() -> String {
    fixture(env!("CARGO_MANIFEST_DIR"), "ext4-basic.img")
}

fn fs(args: &[&str]) -> std::process::Output {
    tool("fs.ext4").args(args).output().expect("spawn fs.ext4")
}

#[test]
fn ls_lists_what_the_kernel_wrote_with_typed_fields() {
    let img = basic();
    let out = ok(tool("fs.ext4").args([&img, "ls", "/"]));
    let json = stdout(&out);
    for (name, kind) in [
        ("test.txt", "file"),
        ("subdir", "dir"),
        ("link.txt", "symlink"),
        ("lost+found", "dir"),
    ] {
        let at = json
            .find(&format!("\"name\": \"{name}\""))
            .unwrap_or_else(|| panic!("{name} not listed:\n{json}"));
        let rest = &json[at..];
        let entry = &rest[..rest.find('}').unwrap()];
        assert_eq!(json_field(entry, "type"), kind, "{entry}");
        for number in ["size", "mtime", "inode"] {
            let v = json_field(entry, number);
            assert!(v.parse::<i64>().is_ok(), "{name}.{number} = {v}");
        }
        let mode = json_field(entry, "mode");
        assert!(
            mode.len() == 4 && mode.chars().all(|c| ('0'..='7').contains(&c)),
            "{name}.mode = {mode}"
        );
    }
    assert!(!json.contains("\"name\": \".\""), "{json}");
    assert!(json.contains("\"target\": \"test.txt\""), "{json}");
    let test_txt = &json[json.find("\"name\": \"test.txt\"").unwrap()..];
    assert_eq!(
        json_field(&test_txt[..test_txt.find('}').unwrap()], "size"),
        "16"
    );

    let text = stdout(&ok(tool("fs.ext4").args([&img, "ls", "--text", "/"])));
    assert!(
        text.lines()
            .any(|l| l.starts_with("l0777") && l.ends_with("link.txt -> test.txt")),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.starts_with("d0755") && l.ends_with(" subdir")),
        "{text}"
    );
}

#[test]
fn ls_of_a_subdirectory_and_of_a_file() {
    let img = basic();
    let sub = stdout(&ok(tool("fs.ext4").args([&img, "ls", "/subdir"])));
    assert!(sub.contains("\"name\": \"nested.txt\""), "{sub}");
    let file = stdout(&ok(tool("fs.ext4").args([
        &img,
        "ls",
        "/subdir/nested.txt",
    ])));
    assert!(file.trim_start().starts_with('['), "{file}");
    assert_eq!(json_field(&file, "name"), "nested.txt");
    assert_eq!(json_field(&file, "size"), "7");
}

#[test]
fn read_writes_the_bytes_the_kernel_wrote() {
    let img = basic();
    let out = ok(tool("fs.ext4").args([&img, "read", "/test.txt"]));
    assert_eq!(out.stdout, b"hello from ext4\n");
    let out = ok(tool("fs.ext4").args([&img, "read", "/subdir/nested.txt"]));
    assert_eq!(out.stdout, b"nested\n");

    let dest = image_path("read-o");
    let out = ok(tool("fs.ext4").args([&img, "read", "/test.txt", "-o", &dest]));
    assert!(out.stdout.is_empty());
    assert_eq!(std::fs::read(&dest).unwrap(), b"hello from ext4\n");
}

#[test]
fn read_refuses_a_directory_a_symlink_and_a_missing_path_with_nothing_on_stdout() {
    let img = basic();
    for (path, says) in [
        ("/subdir", "is a directory"),
        ("/link.txt", "is a symlink to test.txt"),
        ("/no/such/file", "not found"),
    ] {
        let out = fs(&[&img, "read", path]);
        assert_eq!(out.status.code(), Some(1), "{path}");
        assert!(out.stdout.is_empty(), "{path}: {}", stdout(&out));
        let err = stderr(&out);
        assert!(
            err.contains(says) && err.contains("\"code\": 1"),
            "{path}: {err}"
        );
    }
}

#[test]
fn get_and_info_report_the_canonical_keys_and_agree() {
    let img = basic();
    let get = stdout(&ok(tool("fs.ext4").args([&img, "get"])));
    let info = stdout(&ok(tool("fs.ext4").args([&img, "info"])));
    assert_eq!(get, info);
    assert_eq!(json_field(&get, "fs"), "ext4");
    assert_eq!(json_field(&get, "label"), "testvolume");
    assert_eq!(json_field(&get, "block_size"), "4096");
    assert_eq!(json_field(&get, "total_bytes"), (16u64 << 20).to_string());
    assert_eq!(json_field(&get, "dirty"), "false");
    assert!(json_field(&get, "free_bytes").parse::<u64>().is_ok());
    assert!(get.contains("\"ext4\": {"), "{get}");
    assert!(get.contains("\"metadata_csum\""), "{get}");

    let one = stdout(&ok(tool("fs.ext4").args([&img, "get", "label"])));
    assert_eq!(one.trim(), "{\n  \"label\": \"testvolume\"\n}");
    let text = stdout(&ok(tool("fs.ext4").args([&img, "get", "label", "--text"])));
    assert_eq!(text, "testvolume\n");
    let text = stdout(&ok(tool("fs.ext4").args([
        "--text",
        &img,
        "get",
        "ext4.total_blocks",
    ])));
    assert_eq!(text, "4096\n");
}

#[test]
fn an_unknown_key_is_a_usage_error() {
    let out = fs(&[&basic(), "get", "colour"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(
        stderr(&out).contains("no key \\\"colour\\\""),
        "{}",
        stderr(&out)
    );
}

#[test]
fn set_label_and_resize_answer_not_implemented_with_status_3() {
    let img = basic();
    for args in [vec!["set", "label", "X"], vec!["resize", "128M"]] {
        let mut full = vec![img.as_str()];
        full.extend(&args);
        let out = fs(&full);
        assert_eq!(out.status.code(), Some(3), "{args:?}");
        assert!(out.stdout.is_empty());
        let err = stderr(&out);
        assert!(
            err.starts_with("{\"error\": \"not implemented: ") && err.contains("\"code\": 3"),
            "{args:?}: {err}"
        );
    }
    let out = fs(&[&img, "set", "block_size", "1024"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(
        stderr(&out).contains("block_size is read-only"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn offset_reaches_a_partition_inside_a_whole_disk_image() {
    let img = fixture(env!("CARGO_MANIFEST_DIR"), "ext4-whole-disk.img");
    // The partition starts at sector 2048 (guest-build-images.sh).
    let at = (2048 * 512).to_string();
    let label = stdout(&ok(
        tool("fs.ext4").args(["--offset", &at, &img, "get", "label", "--text"])
    ));
    assert_eq!(label, "wholedisk\n");
    let out = ok(tool("fs.ext4").args([&img, "read", "/test.txt", "--offset", &at]));
    assert_eq!(out.stdout, b"whole disk test\n");

    // Without it the partition table is at byte 0, and nothing is an ext4
    // superblock: a structured error, and nothing on stdout.
    let out = fs(&[&img, "ls", "/"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(
        stderr(&out).contains("not a readable ext4 filesystem"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn every_fixture_answers_info_and_ls() {
    for name in [
        "ext4-basic.img",
        "ext4-htree.img",
        "ext4-csum-seed.img",
        "ext4-no-csum.img",
        "ext4-deep-extents.img",
        "ext4-inline.img",
        "ext4-xattr.img",
        "ext4-acl.img",
        "ext4-largedir.img",
        "ext4-manyfiles.img",
    ] {
        let img = fixture(env!("CARGO_MANIFEST_DIR"), name);
        let info = stdout(&ok(tool("fs.ext4").args([&img, "info"])));
        assert_eq!(json_field(&info, "fs"), "ext4", "{name}");
        let ls = stdout(&ok(tool("fs.ext4").args([&img, "ls", "/"])));
        assert!(ls.contains("\"name\": \"lost+found\""), "{name}: {ls}");
    }
}

#[test]
fn a_truncated_image_fails_with_a_structured_error_and_no_output() {
    let bytes = std::fs::read(basic()).unwrap();
    let cut = image_path("truncated");
    std::fs::write(&cut, &bytes[..512]).unwrap();
    for verb in [vec!["ls", "/"], vec!["read", "/test.txt"], vec!["info"]] {
        let mut args = vec![cut.as_str()];
        args.extend(&verb);
        let out = fs(&args);
        assert_eq!(out.status.code(), Some(1), "{verb:?}");
        assert!(out.stdout.is_empty(), "{verb:?}: {}", stdout(&out));
        assert!(
            stderr(&out).contains("\"code\": 1"),
            "{verb:?}: {}",
            stderr(&out)
        );
    }
}
