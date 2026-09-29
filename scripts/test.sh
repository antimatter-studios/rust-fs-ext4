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

# `--features cli` builds the command-line tools (the `rust-fs-ext4`
# target requires it), so every tier reaches their tests. The library a
# consumer links is built without it, and gains nothing from it.
cargo test --features cli "$@"
