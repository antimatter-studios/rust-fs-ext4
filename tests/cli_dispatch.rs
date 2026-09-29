//! The multi-call binary: every name answers as itself, the
//! repository-named form reaches the same tool, `--version` identifies
//! the crate, errors are structured, and `doctor` tells our program from
//! whatever else PATH finds under the same name.
//!
//! No fixture, no VM: the unit tier.

mod cli_support;

use cli_support::*;
use std::path::Path;

const CRATE: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[test]
fn every_name_answers_version_with_itself_the_crate_and_the_version() {
    let mut names = dotted_names();
    assert!(names.contains(&"mkfs.ext4".to_string()), "{names:?}");
    names.push("rust-fs-ext4".to_string());
    for name in names {
        for flag in ["--version", "-V"] {
            let out = ok(tool(&name).arg(flag));
            assert_eq!(
                stdout(&out).trim_end(),
                format!("{name} ({CRATE}) {VERSION}"),
                "{name} {flag}"
            );
        }
    }
}

#[test]
fn the_repository_name_reaches_a_tool_by_verb_and_by_full_name() {
    let img = sized_image("repo-form", 32 << 20);
    let dotted = ok(tool("mkfs.ext4").args(["-n", &img]));
    for word in ["mkfs", "mkfs.ext4"] {
        let repo = ok(tool("rust-fs-ext4").args([word, "-n", &img]));
        assert_eq!(stdout(&repo), stdout(&dotted), "rust-fs-ext4 {word}");
        assert_eq!(stderr(&repo), stderr(&dotted), "rust-fs-ext4 {word}");
    }
    // cargo's own build, under cargo's name, is the same entry point.
    let cargo = ok(entry().args(["mkfs", "-n", &img]));
    assert_eq!(stdout(&cargo), stdout(&dotted));
}

#[test]
fn every_tool_help_carries_an_example() {
    for name in dotted_names() {
        let out = ok(tool(&name).arg("--help"));
        assert!(
            stdout(&out).contains("Examples:"),
            "{name} --help has no example:\n{}",
            stdout(&out)
        );
    }
    let out = ok(tool("rust-fs-ext4").arg("--help"));
    for name in dotted_names() {
        let verb = name.split('.').next().unwrap();
        assert!(
            stdout(&out).contains(&format!("rust-fs-ext4 {verb}")),
            "rust-fs-ext4 --help does not show `rust-fs-ext4 {verb}`:\n{}",
            stdout(&out)
        );
    }
}

#[test]
fn a_wrong_command_line_is_a_structured_error_on_stderr_with_status_2() {
    let out = tool("mkfs.ext4")
        .args(["--no-such-flag", "x.img"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "stdout: {}", stdout(&out));
    let err = stderr(&out);
    assert!(
        err.starts_with("{\"error\": \"") && err.trim_end().ends_with("\"code\": 2}"),
        "{err}"
    );
    assert!(err.contains("--no-such-flag"), "{err}");

    // --text: clap's own message, for a person.
    let out = tool("mkfs.ext4")
        .args(["--text", "--no-such-flag", "x.img"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).starts_with("error: "), "{}", stderr(&out));
}

#[test]
fn a_failed_run_is_a_structured_error_on_stderr_with_status_1() {
    let missing = image_path("never-created");
    let out = tool("mkfs.ext4").arg(&missing).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "stdout: {}", stdout(&out));
    let last = stderr(&out).lines().last().unwrap_or("").to_string();
    assert!(
        last.starts_with("{\"error\": \"open ") && last.ends_with("\"code\": 1}"),
        "{last}"
    );
    let out = tool("mkfs.ext4")
        .args(["--text", &missing])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("mkfs.ext4: open "),
        "{}",
        stderr(&out)
    );
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// A PATH made of `dirs`, and doctor's JSON and status against it.
fn doctor(dirs: &[&Path]) -> (Option<i32>, String) {
    let path = std::env::join_paths(dirs).unwrap();
    let out = entry().arg("doctor").env("PATH", path).output().unwrap();
    (out.status.code(), stdout(&out))
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(fs_ext4_test_support::temp_path!(
        "cli-doctor-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An executable script at `path` that prints `line` for `--version`.
fn impostor(path: &Path, line: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\necho '{line}'\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn doctor_passes_when_every_name_on_path_is_ours() {
    let (code, json) = doctor(&[names_dir()]);
    assert_eq!(code, Some(0), "{json}");
    assert!(json.contains("\"ok\": true"), "{json}");
    for name in dotted_names() {
        assert!(json.contains(&format!("\"name\": \"{name}\"")), "{json}");
    }
    assert!(!json.contains("\"status\": \"missing\""), "{json}");
}

#[test]
fn doctor_names_a_shadowing_program_and_says_which_path_entry_to_move() {
    let theirs = scratch_dir("foreign");
    impostor(&theirs.join("mkfs.ext4"), "mke2fs 1.47.0 (5-Feb-2023)");
    let (code, json) = doctor(&[&theirs, names_dir()]);
    assert_eq!(code, Some(1), "{json}");
    assert!(json.contains("\"ok\": false"), "{json}");
    assert!(json.contains("\"status\": \"foreign\""), "{json}");
    assert!(
        json.contains(&format!(
            "\"path\": \"{}\"",
            theirs.join("mkfs.ext4").display()
        )),
        "{json}"
    );
    assert!(
        json.contains(&format!(
            "put {} before {} on PATH",
            names_dir().display(),
            theirs.display()
        )),
        "{json}"
    );
    // Ours is still found, later, and listed as not run.
    assert!(
        json.contains(&names_dir().join("mkfs.ext4").display().to_string()),
        "{json}"
    );
}

#[test]
fn doctor_names_the_homebrew_formula_to_unlink() {
    let prefix = scratch_dir("brew");
    let real = prefix.join("Cellar/e2fsprogs/1.47.1/sbin/mkfs.ext4");
    impostor(&real, "mke2fs 1.47.1 (20-May-2024)");
    let bin_dir = prefix.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::os::unix::fs::symlink(&real, bin_dir.join("mkfs.ext4")).unwrap();
    let (code, json) = doctor(&[&bin_dir, names_dir()]);
    assert_eq!(code, Some(1), "{json}");
    assert!(json.contains("\"formula\": \"e2fsprogs\""), "{json}");
    assert!(json.contains("`brew unlink e2fsprogs`"), "{json}");
}

#[test]
fn doctor_reports_a_missing_name_with_how_to_install_it() {
    let empty = scratch_dir("empty");
    let (code, json) = doctor(&[&empty]);
    assert_eq!(code, Some(1), "{json}");
    assert!(json.contains("\"status\": \"missing\""), "{json}");
    assert!(json.contains("chore cli:install"), "{json}");
    assert!(
        json.contains("brew install antimatter-studios/tap/rust-fs-ext4"),
        "{json}"
    );
}

#[test]
fn doctor_reports_our_program_at_another_version_as_stale() {
    let old = scratch_dir("stale");
    impostor(
        &old.join("mkfs.ext4"),
        &format!("mkfs.ext4 ({CRATE}) 0.0.1"),
    );
    let (code, json) = doctor(&[&old, names_dir()]);
    assert_eq!(code, Some(1), "{json}");
    assert!(json.contains("\"status\": \"stale\""), "{json}");
    assert!(
        json.contains(&format!("{CRATE} 0.0.1, not {VERSION}")),
        "{json}"
    );
}

#[test]
fn doctor_text_is_for_a_person_and_keeps_the_fix() {
    let theirs = scratch_dir("text");
    impostor(&theirs.join("mkfs.ext4"), "something else entirely");
    let path = std::env::join_paths([theirs.as_path(), names_dir()]).unwrap();
    let out = entry()
        .args(["doctor", "--text"])
        .env("PATH", path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("mkfs.ext4: foreign ("), "{text}");
    assert!(text.contains("  fix: "), "{text}");
    assert!(!text.contains('{'), "{text}");
}
