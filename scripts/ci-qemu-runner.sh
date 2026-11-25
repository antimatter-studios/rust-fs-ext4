#!/usr/bin/env bash
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
operation="${1:?run or prepare required}"
arch="${2:?guest architecture required}"
accelerator="${3:?accelerator required}"
workload="${4:-single}"
case "$operation/$accelerator" in run/tcg|run/kvm|prepare/tcg|prepare/kvm|serve/tcg|serve/kvm) ;; *) echo 'unsupported pipeline runner operation' >&2; exit 2 ;; esac
mkdir -p "$repo/tmp/qemu-benchmark"
python3 "$repo/scripts/ci-qemu-artifacts.py" runner-recipe "$repo" "$(docker info --format '{{.Architecture}}')" \
    >"$repo/tmp/qemu-benchmark/runner-environment.json"
pull_started="$(date +%s)"
image="$(bash "$repo/scripts/ci-qemu-environment.sh" resolve)"
printf '%s\n' "$(( $(date +%s) - pull_started ))" >"$repo/tmp/qemu-benchmark/runner-image-pull-seconds.txt"
[[ "$image" =~ @sha256:[0-9a-f]{64}$ ]] || { echo 'runner must use a GHCR digest' >&2; exit 1; }
printf '%s\n' "$image" >"$repo/tmp/qemu-benchmark/runner-image.txt"
device=()
if [ "$accelerator" = kvm ]; then
    test -r /dev/kvm && test -w /dev/kvm || { echo 'requested KVM device unavailable' >&2; exit 1; }
    device=(--device /dev/kvm)
fi
environment=()
for name in GH_TOKEN GITHUB_ACTOR GITHUB_REPOSITORY GITHUB_RUN_ID GITHUB_RUN_ATTEMPT GITHUB_JOB GITHUB_SHA \
    GITHUB_ACTIONS BENCH_CACHE_REGISTRY BENCH_READY_AT BENCH_JOB_STARTED_AT BENCH_REQUESTED_RUNNER \
    BENCH_SELECTED_RUNNER BENCH_RUNNER_ENVIRONMENT BENCH_RUNNER_NAME QEMU_TCG_CPU QEMU_TCG_ACCEL RUNNER_NAME; do
    if [ -n "${!name:-}" ]; then environment+=(-e "$name"); fi
done
# The runner container gets no CPU or memory limit from this script. The
# platform sets what the runner may use (GitHub for hosted runners, the cluster
# for antimatter-vps pods). The guest VM is what this script limits.
if [ "$operation" = serve ]; then
    if [ -n "${EXT4_CI_FIXTURE_SETUP:-}" ]; then environment+=(-e EXT4_CI_FIXTURE_SETUP); fi
    # CI's VM jobs: `serve x86_64|aarch64 tcg full -- <command...>` runs the
    # command in the runner environment beside a guest (ci-qemu-benchmark.sh
    # serve). The repository and its siblings are mounted at the paths the
    # host knows them by, so what cargo builds here names the paths the guest
    # is given, and the chore this job installed is the chore it runs.
    shift 4
    [ "${1:-}" = -- ] && shift
    [ "$#" -gt 0 ] || { echo 'serve needs -- <command...>' >&2; exit 2; }
    mounts=(-v "$repo:$repo")
    for sibling in rust-fs-core fs-linux-test-harness; do
        if [ -d "$repo/../$sibling" ]; then
            sibling_path="$(cd "$repo/../$sibling" && pwd -P)"
            mounts+=(-v "$sibling_path:$sibling_path")
        fi
    done
    chore_binary="$(command -v chore)" || { echo 'serve runs chore tasks; install chore first' >&2; exit 1; }
    # Through the workspace, which the Docker daemon shares; the runner's home
    # is not shared with it.
    mkdir -p "$repo/tmp/ci-bin"
    cp "$chore_binary" "$repo/tmp/ci-bin/chore"
    mounts+=(-v "$repo/tmp/ci-bin/chore:/usr/local/bin/chore:ro")
    for name in CI GITHUB_WORKSPACE GITHUB_EVENT_NAME GITHUB_REF GITHUB_REF_NAME GITHUB_HEAD_REF \
        GITHUB_BASE_REF GITHUB_SERVER_URL GITHUB_API_URL RUNNER_OS RUNNER_ARCH RUNNER_TEMP \
        CARGO_TERM_COLOR FLTH_RESOURCE_PROFILE OUTPUT_BUDGET_VERBOSE OUTPUT_BUDGET_FAIL_TAIL; do
        if [ -n "${!name:-}" ]; then environment+=(-e "$name"); fi
    done
    # THE RESOURCE PROFILE'S LIMITS. The guest is emulated, and the runner
    # has 4 CPUs: Cargo's default of one job per CPU, and libtest's one
    # thread per CPU, beside the guest's own CPU threads, drove the load
    # past 100 and killed the container and the guest's kernel. The harness
    # says what FLTH_RESOURCE_PROFILE allows (`vm.sh config`; `low` is one
    # build job and one test thread), and the same numbers go to Cargo
    # here, to every test executable cargo runs in the guest
    # (scripts/ci-guest-run.sh passes RUST_* on) and to the in-guest suite
    # (scripts/ci-guest-suite.sh). No profile, no default: it fails.
    harness_vm="$repo/../fs-linux-test-harness/scripts/vm.sh"
    [ -x "$harness_vm" ] || { echo 'serve reads the resource profile from the harness; run chore siblings first' >&2; exit 1; }
    profile_config="$(cd "$repo" && "$harness_vm" config)"
    build_jobs="$(printf '%s\n' "$profile_config" | sed -n 's/^vm\.build_jobs=//p')"
    test_jobs="$(printf '%s\n' "$profile_config" | sed -n 's/^vm\.test_jobs=//p')"
    resource_profile="$(printf '%s\n' "$profile_config" | sed -n 's/^resource\.profile=//p')"
    [[ "$build_jobs" =~ ^[1-9][0-9]*$ && "$test_jobs" =~ ^[1-9][0-9]*$ ]] || {
        echo "serve: the harness gave no build and test job limits (build=$build_jobs test=$test_jobs)" >&2
        exit 1
    }
    echo "serve: resource profile $resource_profile: CARGO_BUILD_JOBS=$build_jobs RUST_TEST_THREADS=$test_jobs"
    environment+=(-e "CARGO_BUILD_JOBS=$build_jobs" -e "FLTH_BUILD_JOBS=$build_jobs"
        -e "FLTH_TEST_JOBS=$test_jobs" -e "RUST_TEST_THREADS=$test_jobs")
    # Cargo builds here and its target runner runs each test executable in
    # the guest (scripts/ci-guest-run.sh): natively for an x86_64 guest, and
    # for an ARM64 guest with the environment's cross toolchain.
    case "$arch" in
        x86_64) environment+=(-e "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER=$repo/scripts/ci-guest-run.sh") ;;
        aarch64)
            environment+=(-e CARGO_BUILD_TARGET=aarch64-unknown-linux-gnu
                -e "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER=$repo/scripts/ci-guest-run.sh"
                -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
                -e CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
                -e AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar) ;;
        *) echo "serve: unsupported guest architecture $arch" >&2; exit 2 ;;
    esac
    # Cargo's registry and git checkouts live in the repository's target/,
    # which the job caches; the toolchain stays the image's own.
    exec docker run --rm "${device[@]+"${device[@]}"}" \
        "${environment[@]+"${environment[@]}"}" -e "BENCH_RUNNER_IMAGE=$image" \
        -e "EVIDENCE_OWNER=$(id -u):$(id -g)" -e "SERVE_REPO=$repo" \
        -e "CARGO_HOME=$repo/target/vps-cargo-home" \
        "${mounts[@]}" -w "$repo" "$image" bash -c '
            set -euo pipefail
            mkdir -p "$CARGO_HOME"
            restore() {
                for owned in target tmp test-disks; do
                    [ ! -e "$SERVE_REPO/$owned" ] || chown -R "$EVIDENCE_OWNER" "$SERVE_REPO/$owned" 2>/dev/null || true
                done
            }
            trap restore EXIT
            git config --global --add safe.directory "*"
            # The oracle tools belong to the guest. Nothing here may use the
            # container'"'"'s own copies: each is replaced with a stub that fails
            # loudly, so a green run is evidence and not a claim.
            for tool in debugfs dumpe2fs e2fsck fsck.ext4 mke2fs mkfs.ext4 tune2fs resize2fs; do
                path="$(command -v "$tool" 2>/dev/null || true)"
                [ -n "$path" ] || continue
                mv "$path" "$path.runner-copy"
                printf "#!/bin/sh\necho \"::error::%s ran in the runner environment; the oracle tools belong in the guest\" >&2\nexit 97\n" "$tool" >"$path"
                chmod +x "$path"
                echo "poisoned $path"
            done
            bash scripts/ci-qemu-benchmark.sh "$@"
        ' -- serve "$arch" "$accelerator" "$workload" -- "$@"
fi
docker run --rm "${device[@]+"${device[@]}"}" \
    "${environment[@]+"${environment[@]}"}" -e "BENCH_RUNNER_IMAGE=$image" \
    -e "EVIDENCE_OWNER=$(id -u):$(id -g)" \
    -v "$repo:/repo" -v "$repo/../rust-fs-core:/rust-fs-core:ro" \
    -w /repo "$image" bash -c '
        set -euo pipefail
        trap '\''chown -R "$EVIDENCE_OWNER" /repo/tmp/qemu-benchmark /repo/tmp/qemu-images 2>/dev/null || true'\'' EXIT
        git config --global --add safe.directory /repo
        git config --global --add safe.directory /rust-fs-core
        bash scripts/ci-qemu-benchmark.sh "$@"
    ' -- "$operation" "$arch" "$accelerator" "$workload"
