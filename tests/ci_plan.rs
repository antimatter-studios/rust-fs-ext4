//! The CI profile's plan and verdict, as a truth table.
//!
//! `scripts/ci-plan.sh` says which profile an event runs: a pull request
//! runs the quick one (unit, lint, semver, msrv, provenance), and every
//! other event, the merge queue, a push, a tag, a manual dispatch, a
//! workflow_call and any event not known here, runs the full one.
//! `scripts/ci-ok.sh` is the `ci-ok` verdict: it accepts a skipped job
//! only where that plan skipped it, and refuses a missing or invalid
//! plan, a missing job, and any failure or cancellation. Both are pinned
//! here row by row so that a profile cannot quietly drop a leg.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const ALL: [&str; 11] = [
    "changes",
    "unit",
    "fixtures",
    "test",
    "test-arm64",
    "suite-in-vm",
    "wasm",
    "semver",
    "msrv",
    "cli",
    "provenance",
];
const FULL_ONLY: [&str; 6] = [
    "fixtures",
    "test",
    "test-arm64",
    "suite-in-vm",
    "wasm",
    "cli",
];
const VM_ONLY: [&str; 4] = ["fixtures", "test", "test-arm64", "suite-in-vm"];

fn script(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join(name)
}

fn plan(event: &str) -> String {
    let out = Command::new("bash")
        .arg(script("ci-plan.sh"))
        .arg(event)
        .output()
        .expect("bash runs scripts/ci-plan.sh");
    assert!(out.status.success(), "ci-plan.sh failed for {event:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn verdict(profile: Option<&str>, code: Option<&str>, results: &[(&str, &str)]) -> bool {
    let mut cmd = Command::new("bash");
    cmd.arg(script("ci-ok.sh"))
        .env_remove("PROFILE")
        .env_remove("CODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(p) = profile {
        cmd.env("PROFILE", p);
    }
    if let Some(c) = code {
        cmd.env("CODE", c);
    }
    let mut child = cmd.spawn().expect("bash runs scripts/ci-ok.sh");
    let mut input = String::new();
    for (job, result) in results {
        input.push_str(&format!("{job} {result}\n"));
    }
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait().unwrap().success()
}

/// What each job reports when the plan is followed exactly.
fn as_planned(profile: &str, code: &str) -> Vec<(&'static str, &'static str)> {
    ALL.iter()
        .map(|&job| {
            let skipped = (profile == "quick" && FULL_ONLY.contains(&job))
                || (code == "false" && VM_ONLY.contains(&job));
            (job, if skipped { "skipped" } else { "success" })
        })
        .collect()
}

fn with(
    mut rows: Vec<(&'static str, &'static str)>,
    job: &str,
    result: &'static str,
) -> Vec<(&'static str, &'static str)> {
    for row in rows.iter_mut() {
        if row.0 == job {
            row.1 = result;
        }
    }
    rows
}

#[test]
fn only_a_pull_request_runs_the_quick_profile() {
    assert_eq!(plan("pull_request"), "quick");
    for event in [
        "merge_group",
        "push",
        "workflow_dispatch",
        "workflow_call",
        "schedule",
        "release",
        "",
        "pull_request_target",
    ] {
        assert_eq!(
            plan(event),
            "full",
            "event {event:?} must run the full profile"
        );
    }
}

#[test]
fn every_planned_outcome_is_green() {
    for profile in ["quick", "full"] {
        for code in ["true", "false"] {
            assert!(
                verdict(Some(profile), Some(code), &as_planned(profile, code)),
                "profile {profile}, code {code}: the planned outcome must pass"
            );
        }
    }
}

#[test]
fn a_full_run_that_skips_a_full_only_job_fails() {
    for job in FULL_ONLY {
        let rows = with(as_planned("full", "true"), job, "skipped");
        assert!(
            !verdict(Some("full"), Some("true"), &rows),
            "full run with {job} skipped must fail"
        );
    }
    for job in ["wasm", "cli"] {
        let rows = with(as_planned("full", "false"), job, "skipped");
        assert!(
            !verdict(Some("full"), Some("false"), &rows),
            "documentation-only full run with {job} skipped must fail"
        );
    }
}

#[test]
fn a_quick_run_never_accepts_a_skipped_quick_job() {
    for job in ["changes", "unit", "semver", "msrv", "provenance"] {
        for profile in ["quick", "full"] {
            let rows = with(as_planned(profile, "true"), job, "skipped");
            assert!(
                !verdict(Some(profile), Some("true"), &rows),
                "{profile} run with {job} skipped must fail"
            );
        }
    }
}

#[test]
fn a_failed_cancelled_or_missing_job_fails_every_profile() {
    for profile in ["quick", "full"] {
        for code in ["true", "false"] {
            for job in ALL {
                for result in ["failure", "cancelled", "", "neutral"] {
                    let rows = with(as_planned(profile, code), job, result);
                    assert!(
                        !verdict(Some(profile), Some(code), &rows),
                        "{profile}/{code}: {job} {result:?} must fail"
                    );
                }
                let rows: Vec<_> = as_planned(profile, code)
                    .into_iter()
                    .filter(|r| r.0 != job)
                    .collect();
                assert!(
                    !verdict(Some(profile), Some(code), &rows),
                    "{profile}/{code}: missing {job} must fail"
                );
            }
        }
    }
}

#[test]
fn a_missing_or_invalid_plan_fails() {
    let rows = as_planned("full", "true");
    assert!(!verdict(None, Some("true"), &rows));
    assert!(!verdict(Some(""), Some("true"), &rows));
    assert!(!verdict(Some("fast"), Some("true"), &rows));
    assert!(!verdict(Some("full"), None, &rows));
    assert!(!verdict(Some("full"), Some("maybe"), &rows));
}
