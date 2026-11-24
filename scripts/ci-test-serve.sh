#!/usr/bin/env bash
set -euo pipefail

[ "${1:-}" = serve ] || { echo 'Expected a native serve command' >&2; exit 2; }
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$repo"
test -r target/ci-test-manifest.json
profile="$(../fs-linux-test-harness/scripts/vm.sh config)"
RUST_TEST_THREADS="$(sed -n 's/^vm\.test_jobs=//p' <<< "$profile")"
CARGO_BUILD_JOBS="$(sed -n 's/^vm\.build_jobs=//p' <<< "$profile")"
[[ "$RUST_TEST_THREADS" =~ ^[1-9][0-9]*$ && "$CARGO_BUILD_JOBS" =~ ^[1-9][0-9]*$ ]] || {
    echo 'Harness resource profile did not specify build and test job limits' >&2
    exit 1
}
export RUST_TEST_THREADS CARGO_BUILD_JOBS FS_EXT4_PREBUILT_REPO="$repo"
exec bash scripts/ci-qemu-benchmark.sh "$@"
