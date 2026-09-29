//! Smoke test for the `mkfs.ext4` (mkfs_ext4) binary.
//!
//! Pre-creates a 32 MiB regular file, runs the binary against it with a
//! known label + UUID, then re-opens the file via the crate's own mount
//! path and verifies the on-disk layout the binary produced is parseable
//! and reflects the CLI args. Catches:
//!   - args plumbed to format_filesystem() correctly (label, UUID propagate)
//!   - file-as-device path opens R/W under the binary's process
//!   - resulting bytes mount cleanly without corruption
//!
//! Stays in tests/ rather than examples/ so `cargo test` runs it as part
//! of the standard suite. The test does NOT require any external tool —
//! it's a pure crate-internal round trip. A matching CI workflow
//! against an external Linux consistency-checker lives in the parent
//! repo's GitHub Actions config when one is wired up.

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use std::process::Command;
use std::sync::Arc;

const SIZE_BYTES: u64 = 32 * 1024 * 1024;
const TEST_LABEL: &str = "BINSMOKE";
const TEST_UUID: &str = "deadbeef-cafe-1234-5678-0123456789ab";

fn unique_tmp_path(suffix: &str) -> std::path::PathBuf {
    // pid + nanos so parallel `cargo test` runs don't clobber each other.
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    fs_ext4_test_support::temp_dir().join(format!("fs-ext4-mkfs-bin-{pid}-{nanos}-{suffix}"))
}

#[test]
fn mkfs_bin_formats_a_pre_sized_file_and_mounts_clean() {
    let bin = env!("CARGO_BIN_EXE_mkfs_ext4");
    let img = unique_tmp_path("img");
    let img_str = img.to_string_lossy().into_owned();

    // Pre-size with std (no `truncate` shell-out — keeps the test
    // platform-portable for when this runs on Windows CI later).
    {
        let f = std::fs::File::create(&img).expect("create img");
        f.set_len(SIZE_BYTES).expect("set_len");
    }

    // Run: mkfs_ext4 -L BINSMOKE -U <uuid> <img>
    let out = Command::new(bin)
        .args(["-L", TEST_LABEL, "-U", TEST_UUID, &img_str])
        .output()
        .expect("spawn mkfs_ext4");

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        panic!(
            "mkfs_ext4 failed: status={:?}\nstderr:\n{stderr}",
            out.status
        );
    }

    // Mount the result via our own read path. If the binary wrote a
    // malformed superblock / BGD / root inode, this will fail.
    let dev = FileDevice::open(&img_str).expect("open formatted image");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount formatted image");

    // Verify args propagated. Label is stored in the superblock's 16-byte
    // volume_name field; check the prefix matches what we passed in.
    assert!(
        fs.sb.volume_name.starts_with(TEST_LABEL),
        "expected volume_name to start with {TEST_LABEL:?}, got {:?}",
        fs.sb.volume_name
    );

    // UUID round-trip — bytes should match the parsed-from-CLI hex string.
    let expected_uuid: [u8; 16] = [
        0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0x12, 0x34, 0x56, 0x78, 0x01, 0x23, 0x45, 0x67, 0x89,
        0xab,
    ];
    assert_eq!(
        fs.sb.uuid, expected_uuid,
        "UUID mismatch — CLI -U argument did not propagate to superblock"
    );

    // Block size defaulted to 4096 (we didn't pass -b).
    assert_eq!(fs.sb.block_size(), 4096);

    // Best-effort cleanup. If this fails (e.g. a preceding panic killed
    // the test before reaching here) the temp file just lingers — fine,
    // the OS reclaims temp_dir eventually and the unique path means no
    // collision next run.
    let _ = std::fs::remove_file(&img);
}

#[test]
fn mkfs_bin_create_size_creates_then_formats() {
    // --create-size end-to-end: point at a non-existent path with
    // --create-size 32M, expect the binary to create + size + format.
    // No prior `truncate` step.
    let bin = env!("CARGO_BIN_EXE_mkfs_ext4");
    let img = unique_tmp_path("createsize");
    let img_str = img.to_string_lossy().into_owned();
    // Make sure the path doesn't exist (test ordering can leave files
    // behind from a previous panic).
    let _ = std::fs::remove_file(&img);

    let out = Command::new(bin)
        .args(["--create-size", "32M", "-L", "CREATED", &img_str])
        .output()
        .expect("spawn mkfs_ext4 --create-size");
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        panic!("mkfs_ext4 --create-size failed: {stderr}");
    }

    // File should exist at exactly 32 MiB (1024-based suffix).
    let meta = std::fs::metadata(&img).expect("formatted file exists");
    assert_eq!(
        meta.len(),
        32 * 1024 * 1024,
        "--create-size should size the file exactly"
    );

    // And it should be a mountable ext4 with our label.
    let dev = FileDevice::open(&img_str).expect("open created image");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount created image");
    assert!(fs.sb.volume_name.starts_with("CREATED"));

    let _ = std::fs::remove_file(&img);
}

#[test]
fn mkfs_bin_create_size_is_idempotent_on_existing_file() {
    // Second invocation against an already-formatted file should
    // succeed (leave-as-is path). Catches regressions where we'd
    // accidentally truncate an existing file back to the requested
    // size, destroying the formatted bytes.
    let bin = env!("CARGO_BIN_EXE_mkfs_ext4");
    let img = unique_tmp_path("idempot");
    let img_str = img.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&img);

    // First call creates + formats.
    let out1 = Command::new(bin)
        .args(["--create-size", "32M", "-L", "FIRST", &img_str])
        .output()
        .expect("spawn mkfs_ext4 first call");
    assert!(out1.status.success(), "first call should succeed");

    // Second call against the same path should NOT explode and should
    // re-format (the binary's contract is "format this thing"; we
    // just don't want it to crash on the metadata check).
    let out2 = Command::new(bin)
        .args(["--create-size", "32M", "-L", "SECOND", &img_str])
        .output()
        .expect("spawn mkfs_ext4 second call");
    assert!(
        out2.status.success(),
        "second call should succeed (idempotent re-format); stderr: {}",
        String::from_utf8_lossy(&out2.stderr)
    );

    // After the second call, the volume label should be SECOND.
    let dev = FileDevice::open(&img_str).expect("open after second format");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount after second format");
    assert!(fs.sb.volume_name.starts_with("SECOND"));

    let _ = std::fs::remove_file(&img);
}

#[test]
fn mkfs_bin_dry_run_does_not_modify_file() {
    let bin = env!("CARGO_BIN_EXE_mkfs_ext4");
    let img = unique_tmp_path("dryrun");
    let img_str = img.to_string_lossy().into_owned();

    // Pre-fill with a recognisable byte pattern — anything other than the
    // ext4 superblock magic at offset 1080 would do, but 0xAA is loud.
    let pattern = vec![0xAAu8; SIZE_BYTES as usize];
    std::fs::write(&img, &pattern).expect("seed pattern");

    let out = Command::new(bin)
        .args(["-n", "-L", "DRYRUN", &img_str])
        .output()
        .expect("spawn mkfs_ext4 -n");
    assert!(
        out.status.success(),
        "dry-run mkfs_ext4 should exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // File contents must be unchanged.
    let after = std::fs::read(&img).expect("read after dry-run");
    assert_eq!(
        after.len(),
        pattern.len(),
        "dry-run must not change file size"
    );
    assert!(
        after == pattern,
        "dry-run must not modify file contents (first diff somewhere)"
    );

    let _ = std::fs::remove_file(&img);
}

// ---------------------------------------------------------------------------
// Flag-parsing behaviour, checked through the built binary.
//
// The unit tests inside src/bin/mkfs_ext4.rs cover the parse table directly.
// These three exist because each of the bugs below was found by running the
// binary and reading what it printed, and what it printed is the part a
// caller actually experiences.
// ---------------------------------------------------------------------------

#[test]
fn mkfs_bin_dash_c_does_not_swallow_the_device_path() {
    // `-c` is boolean in the standard CLI. Parsed as argument-taking, it ate
    // the image path and the tool then reported "missing positional <device>
    // argument" — about the path it had just consumed.
    let bin = env!("CARGO_BIN_EXE_mkfs_ext4");
    let img = unique_tmp_path("dashc");
    let img_str = img.to_string_lossy().into_owned();
    {
        let f = std::fs::File::create(&img).expect("create img");
        f.set_len(SIZE_BYTES).expect("set_len");
    }

    // -n so this stays a parse test and writes nothing.
    let out = Command::new(bin)
        .args(["-n", "-c", &img_str])
        .output()
        .expect("spawn mkfs_ext4 -n -c");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "-c must not consume the device path; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("missing positional"),
        "device path was swallowed; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("-c not yet honored"),
        "-c should still warn that it is ignored; stderr:\n{stderr}"
    );

    let _ = std::fs::remove_file(&img);
}

#[test]
fn mkfs_bin_rejects_bad_block_size_before_opening_the_device() {
    // `-b 3000` is not a power of two. It used to be caught by the formatter,
    // which runs after the device is open read-write and after the tool has
    // printed that it is formatting — so a rejected argument read as a format
    // that failed halfway.
    let bin = env!("CARGO_BIN_EXE_mkfs_ext4");
    let img = unique_tmp_path("badbs");
    let img_str = img.to_string_lossy().into_owned();
    let pattern = vec![0xAAu8; SIZE_BYTES as usize];
    std::fs::write(&img, &pattern).expect("seed pattern");

    let out = Command::new(bin)
        .args(["-b", "3000", &img_str])
        .output()
        .expect("spawn mkfs_ext4 -b 3000");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(!out.status.success(), "-b 3000 must fail");
    assert!(
        stderr.contains("block size must be a power of two"),
        "should name the rule it broke; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("formatting"),
        "must not announce a format it never intended to start; stderr:\n{stderr}"
    );
    assert_eq!(
        std::fs::read(&img).expect("read after rejected -b"),
        pattern,
        "a rejected argument must leave the device untouched"
    );

    let _ = std::fs::remove_file(&img);
}

#[test]
fn mkfs_bin_quiet_silences_warnings_from_either_side() {
    // `-q` was read during the parse loop, so it only silenced flags that
    // came after it: `-q -m 1` was quiet and `-m 1 -q` was not.
    let bin = env!("CARGO_BIN_EXE_mkfs_ext4");
    let img = unique_tmp_path("quiet");
    let img_str = img.to_string_lossy().into_owned();
    {
        let f = std::fs::File::create(&img).expect("create img");
        f.set_len(SIZE_BYTES).expect("set_len");
    }

    let run = |args: &[&str]| -> String {
        let out = Command::new(bin).args(args).output().expect("spawn");
        assert!(out.status.success(), "{args:?} should succeed");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };

    let quiet_first = run(&["-n", "-q", "-m", "1", &img_str]);
    let quiet_last = run(&["-n", "-m", "1", "-q", &img_str]);
    assert_eq!(
        quiet_first, quiet_last,
        "-q must not depend on where it appears"
    );
    assert!(
        quiet_first.is_empty(),
        "-q should silence the ignored-flag warning; got:\n{quiet_first}"
    );

    // And without -q the warning is still there — otherwise the test above
    // would pass just as well on a tool that never warns at all.
    let loud = run(&["-n", "-m", "1", &img_str]);
    assert!(
        loud.contains("-m 1 not yet honored"),
        "warning should appear without -q; got:\n{loud}"
    );

    let _ = std::fs::remove_file(&img);
}

/// `--version` names the crate that built the tool, so this mkfs.ext4 cannot
/// be mistaken for another package's. It said `fs-ext4`, a name no package
/// carries: the crate is published as `am-fs-ext4`.
#[test]
fn mkfs_bin_version_names_the_tool_and_the_crate() {
    let bin = env!("CARGO_BIN_EXE_mkfs_ext4");
    for flag in ["--version", "-V"] {
        let out = Command::new(bin)
            .arg(flag)
            .output()
            .expect("spawn mkfs_ext4");

        assert!(out.status.success(), "{flag} must exit 0: {out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim_end(),
            format!(
                "mkfs.ext4 ({}) {}",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            ),
            "{flag} must print the conventional name, the crate and its version"
        );
    }
}
