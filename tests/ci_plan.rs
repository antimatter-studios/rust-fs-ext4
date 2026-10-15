//! The CI profile's plan and verdict, as a truth table.
//!
//! `scripts/ci-plan.sh` says which profile an event runs: a pull request
//! runs the quick one (unit, lint, semver, msrv, provenance), and every
//! other event, the merge queue, a push, a tag, a manual dispatch, a
//! workflow_call and any event not known here, runs the full one.
//! `scripts/ci-ok.sh` is the `ci-ok` verdict: it accepts a skipped job
//! only where that plan skipped it, and refuses a missing or invalid
//! plan, a missing job, and any failure or cancellation.
//!
//! A test here spawns no shell (tests/test_contract.rs), so the rows are
//! executed by `scripts/test-ci-plan.sh`, which the `changes` job runs on
//! every event before it plans the profile. This file pins the schema
//! both sides share, the job lists in ci-ok.sh, the guard and the
//! workflow, and that the guard grades every row family named below, so
//! that a profile cannot quietly drop a leg.

use std::path::PathBuf;

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

fn read(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn guard() -> String {
    read("scripts/test-ci-plan.sh")
}

/// The words of a `name=(...)` bash array or a `name="..."` list.
fn list(text: &str, opener: &str, closer: char) -> Vec<String> {
    let at = text
        .find(opener)
        .unwrap_or_else(|| panic!("no {opener:?} list"));
    let rest = &text[at + opener.len()..];
    let end = rest.find(closer).expect("list is closed");
    rest[..end].split_whitespace().map(str::to_string).collect()
}

fn names(jobs: &[&str]) -> Vec<String> {
    jobs.iter().map(|j| j.to_string()).collect()
}

/// The guard runs in the `changes` job, before the profile is planned,
/// so no event can plan a profile whose truth table did not hold.
fn assert_guard_is_wired() {
    let ci = read(".github/workflows/ci.yml");
    let grade = ci
        .find("run: bash scripts/test-ci-plan.sh")
        .expect("ci.yml runs scripts/test-ci-plan.sh");
    let changes = ci.find("\n  changes:\n").expect("ci.yml has a changes job");
    let plan = ci
        .find("profile=\"$(bash scripts/ci-plan.sh \"$EVENT\")\"")
        .expect("ci.yml plans the profile with scripts/ci-plan.sh");
    assert!(
        changes < grade && grade < plan,
        "the guard must run in changes, before the plan"
    );
    assert!(ci.contains("EVENT: ${{ github.event_name }}"));
}

fn assert_guard_grades(rows: &[&str]) {
    assert_guard_is_wired();
    let text = guard();
    for row in rows {
        assert!(
            text.contains(row),
            "scripts/test-ci-plan.sh no longer grades {row:?}"
        );
    }
    assert!(text
        .trim_end()
        .ends_with("echo \"test-ci-plan.sh: all checks passed\""));
}

#[test]
fn only_a_pull_request_runs_the_quick_profile() {
    assert_guard_grades(&[
        "ci-plan.sh\" pull_request)\" && [[ \"$got\" == quick ]]",
        "for event in merge_group push workflow_dispatch workflow_call schedule release \"\" pull_request_target; do",
        "[[ \"$got\" == full ]]",
    ]);
}

#[test]
fn every_planned_outcome_is_green() {
    let text = guard();
    assert_eq!(list(&text, "\nALL=(", ')'), names(&ALL));
    assert_eq!(list(&text, "\nFULL_ONLY=(", ')'), names(&FULL_ONLY));
    assert_eq!(list(&text, "\nVM_ONLY=(", ')'), names(&VM_ONLY));
    let ok = read("scripts/ci-ok.sh");
    assert_eq!(list(&ok, "\nall=\"", '"'), names(&ALL));
    assert_eq!(list(&ok, "\nfull_only=\"", '"'), names(&FULL_ONLY));
    assert_eq!(list(&ok, "\nvm_only=\"", '"'), names(&VM_ONLY));
    let ci = read(".github/workflows/ci.yml");
    let needs: Vec<String> = list(
        &ci[ci.find("\n  ci-ok:\n").expect("ci.yml has a ci-ok job")..],
        "\n    needs: [changes,",
        ']',
    )
    .iter()
    .map(|j| j.trim_end_matches(',').to_string())
    .collect();
    assert_eq!(
        needs,
        names(&ALL[1..]),
        "ci-ok needs every job the verdict reads"
    );
    assert_guard_grades(&[
        "expect pass \"profile $profile, code $code: the planned outcome\" \"$profile\" \"$code\" \"$profile\" \"$code\"",
        "{ [[ \"$profile\" == quick ]] && in_list \"$job\" \"${FULL_ONLY[@]}\"; } ||",
        "{ [[ \"$code\" == false ]] && in_list \"$job\" \"${VM_ONLY[@]}\"; }; then",
    ]);
}

#[test]
fn a_full_run_that_skips_a_full_only_job_fails() {
    assert_guard_grades(&[
        "for job in \"${FULL_ONLY[@]}\"; do\n  expect fail \"full run with $job skipped\" full true full true \"$job\" skipped",
        "for job in wasm cli; do\n  expect fail \"documentation-only full run with $job skipped\" full false full false \"$job\" skipped",
    ]);
}

#[test]
fn a_quick_run_never_accepts_a_skipped_quick_job() {
    let text = guard();
    assert_eq!(
        list(&text, "\nQUICK=(", ')'),
        names(&["changes", "unit", "semver", "msrv", "provenance"])
    );
    assert_guard_grades(&[
        "for job in \"${QUICK[@]}\"; do\n  for profile in quick full; do\n    expect fail \"$profile run with $job skipped\" \"$profile\" true \"$profile\" true \"$job\" skipped",
    ]);
}

#[test]
fn a_failed_cancelled_or_missing_job_fails_every_profile() {
    assert_guard_grades(&[
        "for result in failure cancelled \"\" neutral; do",
        "expect fail \"$profile/$code: $job '$result'\" \"$profile\" \"$code\" \"$profile\" \"$code\" \"$job\" \"$result\"",
        "expect fail \"$profile/$code: missing $job\" \"$profile\" \"$code\" \"$profile\" \"$code\" \"$job\" --omit",
    ]);
}

#[test]
fn a_missing_or_invalid_plan_fails() {
    assert_guard_grades(&[
        "expect fail \"unset profile\" \"<unset>\" true full true",
        "expect fail \"empty profile\" \"\" true full true",
        "expect fail \"profile fast\" fast true full true",
        "expect fail \"unset code\" full \"<unset>\" full true",
        "expect fail \"code maybe\" full maybe full true",
    ]);
}
