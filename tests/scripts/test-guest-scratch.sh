#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
runner="$REPO/scripts/guest-scratch.sh"
mkdir -p "$REPO/tmp"
sandbox="$(mktemp -d "$REPO/tmp/guest-scratch-check.XXXXXX")"
trap 'find "$sandbox" -depth -mindepth 1 -delete; rmdir "$sandbox"' EXIT
export SCRATCH_TEST_ROOT="$sandbox"
mkdir "$sandbox/repo"

fail() { echo "FAIL  $*" >&2; exit 1; }

mount() {
    [ "$#" = 3 ] && [ "$1" = --bind ] && [ "$3" = "$SCRATCH_TEST_ROOT/repo/tmp" ] || return 82
    case "$2" in "$SCRATCH_TEST_ROOT"/local.*) ;; *) return 83 ;; esac
    printf 'mount\n' >> "$SCRATCH_TEST_ROOT/trace"
    return "${SCRATCH_TEST_MOUNT_STATUS:-0}"
}
umount() {
    [ "$#" = 1 ] && [ "$1" = "$SCRATCH_TEST_ROOT/repo/tmp" ] || return 84
    printf 'unmount\n' >> "$SCRATCH_TEST_ROOT/trace"
    return "${SCRATCH_TEST_UMOUNT_STATUS:-0}"
}
mktemp() {
    [ "$#" = 2 ] && [ "$1" = -d ] && [ "$2" = /var/tmp/fs-ext4-tests.XXXXXX ] || return 85
    local directory
    directory="$(command mktemp -d "$SCRATCH_TEST_ROOT/local.XXXXXX")"
    printf '%s\n' "$directory" > "$SCRATCH_TEST_ROOT/source"
    printf '%s\n' "$directory"
}
export -f mount umount mktemp
bash -c 'declare -F mount && declare -F umount && declare -F mktemp' >/dev/null ||
    fail "the mount stubs are not inherited; refusing to run the probes"

run() {
    : > "$sandbox/trace"
    (cd "$sandbox/repo" && FLTH_GUEST=1 bash "$runner" "$@") > "$sandbox/output" 2>&1
}

if (cd "$sandbox/repo" && unset FLTH_GUEST && bash "$runner" true) > "$sandbox/output" 2>&1; then
    fail "the host invocation was accepted"
fi
grep -q 'INSIDE the harness VM' "$sandbox/output" || fail "the host refusal was not explained"
[ ! -e "$sandbox/source" ] || fail "the host invocation allocated scratch"

run bash -c 'printf "command %s|%s\n" "$1" "$2" >> "$SCRATCH_TEST_ROOT/trace"' command 'with spaces' two ||
    fail "the successful command failed: $(cat "$sandbox/output")"
expected="$(printf 'mount\ncommand with spaces|two\nunmount')"
[ "$(cat "$sandbox/trace")" = "$expected" ] || fail "mount, arguments, or unmount order changed"
[ ! -d "$(cat "$sandbox/source")" ] || fail "empty scratch survived success"

status=0
run bash -c 'printf "failed command\n" >> "$SCRATCH_TEST_ROOT/trace"; exit 37' || status=$?
[ "$status" = 37 ] || fail "command status 37 became $status"
[ "$(tail -1 "$sandbox/trace")" = unmount ] || fail "failure did not unmount"
[ ! -d "$(cat "$sandbox/source")" ] || fail "empty scratch survived command failure"

status=0
run bash -c 'kill -TERM "$PPID"' || status=$?
[ "$status" = 143 ] || fail "termination status became $status"
[ "$(tail -1 "$sandbox/trace")" = unmount ] || fail "termination did not unmount"

run bash -c 'printf kept > "$(cat "$SCRATCH_TEST_ROOT/source")/image"' ||
    fail "retaining an image failed the command"
[ "$(cat "$(cat "$sandbox/source")/image")" = kept ] || fail "retained scratch was removed"
grep -q 'scratch retained at' "$sandbox/output" || fail "retained scratch was not reported"

export SCRATCH_TEST_MOUNT_STATUS=77
status=0
run bash -c 'echo command >> "$SCRATCH_TEST_ROOT/trace"' || status=$?
unset SCRATCH_TEST_MOUNT_STATUS
[ "$status" = 77 ] || fail "mount status 77 became $status"
[ "$(cat "$sandbox/trace")" = mount ] || fail "a failed mount ran the command or unmounted"
[ ! -d "$(cat "$sandbox/source")" ] || fail "empty scratch survived mount failure"

export SCRATCH_TEST_UMOUNT_STATUS=71
status=0
run true || status=$?
[ "$status" -ne 0 ] || fail "unmount failure was reported as success"
[ -d "$(cat "$sandbox/source")" ] || fail "scratch was removed while still mounted"
grep -q 'could not unmount' "$sandbox/output" || fail "unmount failure was not explained"
status=0
run bash -c 'exit 37' || status=$?
unset SCRATCH_TEST_UMOUNT_STATUS
[ "$status" = 37 ] || fail "unmount failure hid the command failure"

if (cd "$sandbox/repo" && FLTH_GUEST=1 bash "$runner") > "$sandbox/output" 2>&1; then
    fail "a missing command was accepted"
fi

echo "PASS  guest scratch isolates the command, preserves arguments and failures, and unmounts on exit"
