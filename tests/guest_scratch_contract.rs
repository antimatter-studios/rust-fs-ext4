#[test]
fn the_guest_suite_runs_unchanged_tests_with_guest_local_scratch() {
    for runner in [
        include_str!("../scripts/guest-suite.sh"),
        include_str!("../scripts/ci-guest-compile-suite.sh"),
    ] {
        assert!(
            runner.contains("bash scripts/guest-scratch.sh scripts/test.sh --locked --profile test-opt \"$@\""),
            "the guest suite must isolate scratch from the repository share before running the original tests"
        );
    }
}

#[test]
fn the_harness_guest_runs_what_the_host_built_and_never_compiles() {
    let runner = include_str!("../scripts/guest-suite.sh");
    assert!(runner.contains(
        "scripts/ci-test-bundle.py install \"$arch\" \"$key\" \"$build/bundle.tar\" local"
    ));
    assert!(runner.contains("export FS_EXT4_PREBUILT_REPO=\"$PWD\""));
    // What it runs, not what its comments say.
    let commands: Vec<&str> = runner
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect();
    for compiles in ["guest-rust-toolchain.sh", "cargo ", "rustup"] {
        assert!(
            !commands.iter().any(|line| line.contains(compiles)),
            "scripts/guest-suite.sh must not reach a toolchain ({compiles}): the harness's [build] step compiles"
        );
    }
    let config = include_str!("../fs-linux-test-harness.toml");
    assert!(config.contains("dockerfile = \"scripts/ci-test-builder/Dockerfile\""));
    assert!(config.contains("context = \"scripts/test-builder-context.sh\""));
    assert!(config.contains("command = \"scripts/test-bundle-build.sh local\""));
    // CI's image and the local one are made from one recipe.
    assert!(include_str!("../scripts/ci-test-builder-image.sh")
        .contains("bash scripts/test-builder-context.sh \"$scratch/recipe\""));
    assert!(include_str!("../scripts/ci-test-compile.sh")
        .contains("bash scripts/test-bundle-build.sh ci \"$1\" \"$2\" /bundle/bundle.tar --lint"));
}

#[test]
fn the_served_suite_limits_optimizer_work_only_inside_the_guest() {
    let served = include_str!("../scripts/ci-guest-suite.sh");
    assert!(
        served.contains(
            "command=\"cd /repo && FLTH_GUEST=1$limits CARGO_PROFILE_TEST_OPT_OPT_LEVEL=1 exec bash scripts/ci-guest-compile-suite.sh\""
        ),
        "the served suite must send the lower optimization level to the guest Cargo process without changing its resource limits"
    );
    assert!(!include_str!("../scripts/ci-guest-compile-suite.sh")
        .contains("CARGO_PROFILE_TEST_OPT_OPT_LEVEL"));
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
