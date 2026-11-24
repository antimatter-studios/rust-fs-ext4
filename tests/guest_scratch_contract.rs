#[test]
fn the_guest_suite_runs_unchanged_tests_with_guest_local_scratch() {
    let runner = include_str!("../scripts/guest-suite.sh");
    assert!(
        runner.contains("bash scripts/guest-scratch.sh scripts/test.sh --locked --profile test-opt \"$@\""),
        "the guest suite must isolate scratch from the repository share before running the original tests"
    );
}

#[test]
fn the_served_suite_limits_optimizer_work_only_inside_the_guest() {
    let served = include_str!("../scripts/ci-guest-suite.sh");
    assert!(
        served.contains(
            "command=\"cd /repo && FLTH_GUEST=1$limits CARGO_PROFILE_TEST_OPT_OPT_LEVEL=1 exec bash scripts/guest-suite.sh\""
        ),
        "the served suite must send the lower optimization level to the guest Cargo process without changing its resource limits"
    );
    assert!(!include_str!("../scripts/guest-suite.sh").contains("CARGO_PROFILE_TEST_OPT_OPT_LEVEL"));
}

#[test]
fn guest_scratch_is_local_guarded_and_unmounted() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/guest-scratch.sh");
    let runner = std::fs::read_to_string(path).expect("the guest scratch runner must exist");
    for required in [
        "FLTH_GUEST",
        "/var/tmp/",
        "mount --bind",
        "umount",
        "trap",
        "\"$@\"",
    ] {
        assert!(
            runner.contains(required),
            "guest scratch runner is missing {required}"
        );
    }
}
