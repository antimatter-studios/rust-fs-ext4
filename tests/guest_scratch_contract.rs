#[test]
fn the_guest_suite_runs_unchanged_tests_with_guest_local_scratch() {
    let runner = include_str!("../scripts/guest-suite.sh");
    assert!(
        runner.contains("bash scripts/guest-scratch.sh scripts/test.sh --locked --release \"$@\""),
        "the guest suite must isolate scratch from the repository share before running the original tests"
    );
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
