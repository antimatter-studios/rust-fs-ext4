//! `mkfs.ext4` as the multi-call binary runs it: the behaviour the
//! `mkfs_ext4` target had, kept (the checks from tests/mkfs_bin_smoke.rs,
//! run under the dotted name), and the JSON report it gained.
//!
//! No fixture, no VM: the volume is read back with this crate's reader.
//! What e2fsprogs makes of the same output is tests/cli_mkfs_oracle.rs.

mod cli_support;

use cli_support::*;
use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use std::sync::Arc;

const SIZE: u64 = 32 << 20;
const UUID: &str = "deadbeef-cafe-1234-5678-0123456789ab";

fn mount(path: &str) -> Filesystem {
    let dev = FileDevice::open(path).expect("open image");
    Filesystem::mount(Arc::new(dev)).expect("mount")
}

#[test]
fn formats_a_pre_sized_file_and_reports_what_the_superblock_says() {
    let img = sized_image("presized", SIZE);
    let out = ok(tool("mkfs.ext4").args(["-L", "BINSMOKE", "-U", UUID, &img]));
    let json = stdout(&out);
    assert_eq!(json_field(&json, "fs"), "ext4");
    assert_eq!(json_field(&json, "formatted"), "true");
    assert_eq!(json_field(&json, "label"), "BINSMOKE");
    assert_eq!(json_field(&json, "uuid"), UUID);

    let fs = mount(&img);
    assert_eq!(fs.sb.volume_name, "BINSMOKE");
    assert_eq!(
        json_field(&json, "block_size"),
        fs.sb.block_size().to_string()
    );
    assert_eq!(
        json_field(&json, "total_blocks"),
        fs.sb.blocks_count.to_string()
    );
    assert_eq!(
        json_field(&json, "free_blocks"),
        fs.sb.free_blocks_count.to_string()
    );
    assert_eq!(
        json_field(&json, "total_bytes"),
        (fs.sb.blocks_count * u64::from(fs.sb.block_size())).to_string()
    );
    // Progress is still on stderr.
    assert!(
        stderr(&out).contains("formatted successfully"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn text_prints_nothing_on_stdout_as_the_tool_always_did() {
    let img = sized_image("text", SIZE);
    let out = ok(tool("mkfs.ext4").args(["--text", &img]));
    assert!(out.stdout.is_empty(), "{}", stdout(&out));
}

#[test]
fn size_creates_then_formats_and_create_size_is_its_alias() {
    for flag in ["--size", "--create-size"] {
        let img = image_path("size");
        ok(tool("mkfs.ext4").args([flag, "32M", "-L", "SZ", &img]));
        assert_eq!(std::fs::metadata(&img).unwrap().len(), SIZE, "{flag}");
        assert_eq!(mount(&img).sb.volume_name, "SZ", "{flag}");
    }
}

#[test]
fn size_is_idempotent_on_an_existing_file() {
    let img = sized_image("idem", SIZE);
    let out = ok(tool("mkfs.ext4").args(["--size", "64M", &img]));
    assert_eq!(std::fs::metadata(&img).unwrap().len(), SIZE);
    assert!(stderr(&out).contains("already exists"), "{}", stderr(&out));
}

#[test]
fn dry_run_does_not_modify_the_file() {
    let img = image_path("dry");
    let pattern = vec![0xA5u8; SIZE as usize];
    std::fs::write(&img, &pattern).unwrap();
    let out = ok(tool("mkfs.ext4").args(["-n", &img]));
    assert_eq!(std::fs::read(&img).unwrap(), pattern);
    assert_eq!(json_field(&stdout(&out), "formatted"), "false");
    assert_eq!(json_field(&stdout(&out), "dry_run"), "true");
}

#[test]
fn dry_run_with_size_does_not_create_a_missing_target() {
    let img = image_path("drysize");
    let out = ok(tool("mkfs.ext4").args(["-n", "--size", "32M", &img]));
    assert!(std::fs::metadata(&img).is_err(), "a dry run created {img}");
    let json = stdout(&out);
    assert_eq!(json_field(&json, "formatted"), "false");
    assert_eq!(json_field(&json, "dry_run"), "true");
    assert_eq!(json_field(&json, "device_bytes"), SIZE.to_string());
}

#[test]
fn dash_c_does_not_swallow_the_device_path() {
    let img = sized_image("dashc", SIZE);
    let out = ok(tool("mkfs.ext4").args(["-n", "-c", &img]));
    assert!(
        stderr(&out).contains("-c not yet honored"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_bad_block_size_is_refused_before_the_device_is_opened() {
    let img = image_path("badbs");
    let pattern = vec![0xAAu8; SIZE as usize];
    std::fs::write(&img, &pattern).unwrap();
    let out = tool("mkfs.ext4")
        .args(["-b", "3000", &img])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("block size must be a power of two"), "{err}");
    assert!(!err.contains("formatting"), "{err}");
    assert_eq!(std::fs::read(&img).unwrap(), pattern);
}

#[test]
fn a_non_ascii_uuid_is_a_usage_error_not_a_panic() {
    // 32 bytes, so the length check passes, with a two-byte character
    // straddling the first hex pair.
    let uuid = format!("a\u{e9}{}", "0".repeat(29));
    assert_eq!(uuid.len(), 32);
    let img = sized_image("uuid-utf8", SIZE);
    let out = tool("mkfs.ext4")
        .args(["-n", "-U", &uuid, &img])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(!stderr(&out).contains("panicked"), "{}", stderr(&out));
}

#[test]
fn quiet_silences_warnings_from_either_side() {
    let img = sized_image("quiet", SIZE);
    let run = |args: &[&str]| stderr(&ok(tool("mkfs.ext4").args(args)));
    let first = run(&["-n", "-q", "-m", "1", &img]);
    let last = run(&["-n", "-m", "1", "-q", &img]);
    assert_eq!(first, last);
    assert!(first.is_empty(), "{first}");
    assert!(run(&["-n", "-m", "1", &img]).contains("-m 1 not yet honored"));
}

#[test]
fn a_block_device_is_refused_by_size() {
    // /dev/null is a character device on every Unix: "make me a file" must
    // not be applied to it.
    let out = tool("mkfs.ext4")
        .args(["--size", "1M", "/dev/null"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("refuses to apply"),
        "{}",
        stderr(&out)
    );
}
