#!/usr/bin/env bash
# test.sh [cargo test args...]  run the suite in an owned scratch directory
# test.sh --print-temp-dir      print that directory and exit
#
# SCRATCH LIVES IN THE REPOSITORY, always: tmp/ (gitignored), and never
# the system temporary directory or a runner-supplied one. The oracle
# tools run inside the fs-linux-test-harness VM, which sees this
# repository at the path the host knows it by and nothing else of the
# host — so an image anywhere else is a path the tool asked to read it
# cannot open. The same rule is written in Rust in
# tests/support/src/lib.rs (select_temp_dir), and
# tests/test_temp_policy.rs checks it.
#
# FS_EXT4_TEST_TMPDIR supplies an exact directory instead; it must be
# inside the repository, and it is the caller's to delete.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR=""

cleanup() {
    if [[ -n "$RUN_DIR" && -d "$RUN_DIR" ]]; then
        find "$RUN_DIR" -depth -mindepth 1 -delete
        rmdir "$RUN_DIR"
    fi
}
trap cleanup EXIT HUP INT TERM

if [[ -n "${FS_EXT4_TEST_TMPDIR:-}" ]]; then
    case "$FS_EXT4_TEST_TMPDIR" in
        "$REPO"/*) ;;
        *)
            echo "test.sh: FS_EXT4_TEST_TMPDIR is $FS_EXT4_TEST_TMPDIR, which is outside" >&2
            echo "         $REPO. The oracle tools run in the harness VM, which sees this" >&2
            echo "         repository and nothing else of the host." >&2
            exit 1
            ;;
    esac
    # An exact caller-supplied directory is not ours to delete.
    mkdir -p "$FS_EXT4_TEST_TMPDIR"
else
    mkdir -p "$REPO/tmp"
    RUN_DIR="$(mktemp -d "$REPO/tmp/fs-ext4-tests.XXXXXX")"
    export FS_EXT4_TEST_TMPDIR="$RUN_DIR"
fi

export TMPDIR="$FS_EXT4_TEST_TMPDIR"

if [[ "${1:-}" == "--print-temp-dir" ]]; then
    printf '%s\n' "$FS_EXT4_TEST_TMPDIR"
    exit 0
fi

# THE RUN OWNS THE VM ITS TESTS BOOT. An oracle or kernel test brings the
# harness VM up from inside its own process, and that boot takes the
# machine-wide slot, ONE for every repository on the machine. Left to
# chore's `after_all` reaper, which runs only inside a chore invocation of
# this repository, a run that reached cargo any other way (this script by
# hand, scripts/tier.sh) exited with the VM idle and the slot held, and
# every other repository's VM work queued behind it until the guest's idle
# deadline. `vm.sh session` runs cargo inside a harness session: the VM
# comes down and the slot is released when the run ends, passed, failed or
# killed, and a VM held with `chore vm:up` is left up. In the guest, and on
# a host that cannot run the VM, it runs cargo as it is. Without the
# harness sibling no test can boot a VM, so there is none to own.
session=()
HARNESS_VM="$REPO/../fs-linux-test-harness/scripts/vm.sh"
[[ -x "$HARNESS_VM" ]] && session=("$HARNESS_VM" session)

# `--features cli` builds the command-line tools (the `rust-fs-ext4`
# target requires it), so every tier reaches their tests. The library a
# consumer links is built without it, and gains nothing from it.
#
# The `+` form because an empty array is an unbound variable to bash
# before 4.4, which is what macOS ships.
${session[@]+"${session[@]}"} cargo test --features cli "$@"
