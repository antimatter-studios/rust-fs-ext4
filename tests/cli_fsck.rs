//! `fsck.ext4`'s verdicts and fsck(8) exit statuses, on images our own
//! `mkfs.ext4` makes and then damages: clean (0), a wrong free-blocks
//! count in a group descriptor found (4) and repaired (1, then 0), an
//! image that cannot be opened (8), and a wrong command line (16). A
//! destroyed root-directory extent header should be 4 as well; the audit
//! skips a directory it cannot read today (#445), so that case checks the
//! other verbs only, and gains its fsck assertion when #445 lands.
//!
//! What e2fsck makes of the same images is tests/cli_fsck_oracle.rs.

mod cli_support;

use cli_support::*;

fn fresh(tag: &str) -> String {
    let img = image_path(tag);
    ok(tool("mkfs.ext4").args(["-q", "--size", "32M", "--text", &img]));
    img
}

fn fsck(args: &[&str]) -> (Option<i32>, String) {
    let out = tool("fsck.ext4")
        .args(args)
        .output()
        .expect("spawn fsck.ext4");
    (out.status.code(), stdout(&out))
}

#[test]
fn a_fresh_image_is_clean_and_status_0() {
    let img = fresh("clean");
    let (code, json) = fsck(&[&img]);
    assert_eq!(code, Some(0), "{json}");
    assert_eq!(json_field(&json, "clean"), "true");
    assert_eq!(json_field(&json, "mode"), "check");
    assert_eq!(json_field(&json, "found"), "0");
    assert_eq!(json_field(&json, "exit"), "0");
    let (code, text) = fsck(&["--text", "-fn", &img]);
    assert_eq!(code, Some(0));
    assert!(text.contains(": clean ("), "{text}");
}

#[test]
fn a_wrong_group_free_count_is_found_then_repaired_then_clean() {
    let img = fresh("drift");
    corrupt_group_free_blocks(&img, 7);
    let before = std::fs::read(&img).unwrap();

    let (code, json) = fsck(&["-n", &img]);
    assert_eq!(code, Some(4), "{json}");
    assert_eq!(json_field(&json, "clean"), "false");
    assert!(
        json.contains("\"kind\": \"group_free_count_drift\""),
        "{json}"
    );
    assert_eq!(std::fs::read(&img).unwrap(), before, "-n changed the image");

    let (code, json) = fsck(&["-y", &img]);
    assert_eq!(code, Some(1), "{json}");
    assert_eq!(json_field(&json, "mode"), "repair");
    assert_eq!(json_field(&json, "remaining"), "0");

    let (code, json) = fsck(&[&img]);
    assert_eq!(code, Some(0), "a second run after the repair: {json}");
}

#[test]
fn preen_repairs_as_yes_does() {
    for flag in ["-p", "-a"] {
        let img = fresh("preen");
        corrupt_group_free_blocks(&img, 3);
        let (code, json) = fsck(&[flag, &img]);
        assert_eq!(code, Some(1), "{flag}: {json}");
        assert_eq!(fsck(&[&img]).0, Some(0), "{flag}");
    }
}

#[test]
fn every_verb_on_a_destroyed_root_fails_with_a_structured_error() {
    let img = fresh("rootless");
    destroy_root_extent_header(&img);
    // No panic and no partial garbage: status 1, a JSON error on stderr,
    // nothing on stdout. (fsck.ext4's own verdict on this image: #445.)
    for verb in [
        vec!["ls", "/"],
        vec!["read", "/x"],
        vec!["ls", "/lost+found"],
    ] {
        let mut args = vec![img.as_str()];
        args.extend(&verb);
        let out = tool("fs.ext4").args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{verb:?}: {}", stderr(&out));
        assert!(out.stdout.is_empty(), "{verb:?}: {}", stdout(&out));
        assert!(
            stderr(&out).starts_with("{\"error\": "),
            "{verb:?}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn an_image_that_cannot_be_opened_is_status_8() {
    let missing = image_path("never-made");
    let out = tool("fsck.ext4").arg(&missing).output().unwrap();
    assert_eq!(out.status.code(), Some(8));
    assert!(out.stdout.is_empty());
    assert!(stderr(&out).contains("\"code\": 8"), "{}", stderr(&out));

    let img = fresh("truncated");
    let bytes = std::fs::read(&img).unwrap();
    std::fs::write(&img, &bytes[..2048]).unwrap();
    let out = tool("fsck.ext4").arg(&img).output().unwrap();
    assert_eq!(out.status.code(), Some(8), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
}

#[test]
fn a_wrong_command_line_is_status_16() {
    let img = fresh("usage");
    for args in [
        vec!["-n", "-y", img.as_str()],
        vec!["--bogus", img.as_str()],
        vec![],
    ] {
        let out = tool("fsck.ext4").args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(16), "{args:?}: {}", stderr(&out));
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}

/// A target is opened by its bytes, not by a lossy rendering of them: a
/// name that is not UTF-8 must not open the file whose name is its
/// replacement-character spelling, read-only or read-write.
#[test]
fn a_target_that_is_not_utf8_is_opened_by_its_own_bytes() {
    use std::os::unix::ffi::OsStrExt;
    let dir = image_path("non-utf8-dir");
    std::fs::create_dir_all(&dir).unwrap();
    let decoy = std::path::Path::new(&dir).join("img\u{FFFD}");
    std::fs::rename(fresh("decoy"), &decoy).unwrap();
    let target = std::path::Path::new(&dir).join(std::ffi::OsStr::from_bytes(b"img\xff"));
    for (name, args) in [("fs.ext4", ["info"]), ("fsck.ext4", ["-y"])] {
        let mut cmd = tool(name);
        cmd.arg(&target).args(args);
        let out = cmd.output().expect("spawn");
        assert!(
            !out.status.success() && out.stdout.is_empty(),
            "{cmd:?} opened the decoy {decoy:?}\nstdout:\n{}",
            stdout(&out)
        );
    }
}
