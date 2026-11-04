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
trap cleanup EXIT

# A SIGNAL STOPS THE RUN. Until the command exists there is nothing to pass it
# to, so it is only recorded, and the command is not started. Once it does, it
# goes to the command's process alone: with a session that is the session,
# which passes it on and then brings the VM down, and whose teardown a signal to
# its whole process group would kill. stop_run deals with the rest of the group.
CHILD_PID=""
STOP_SIGNAL=""
STOP_STATUS=""
note_signal() {
    STOP_SIGNAL="$1"
    STOP_STATUS=$((128 + $2))
    if [[ -n "$CHILD_PID" ]]; then
        kill -s "$1" "$CHILD_PID" 2>/dev/null || true
    fi
}
trap 'note_signal HUP 1' HUP
trap 'note_signal INT 2' INT
trap 'note_signal TERM 15' TERM

# gone_within SECONDS TARGET: kill's TARGET, a process or a -group, has no
# process left in it, waiting up to SECONDS.
gone_within() {
    local limit=$((SECONDS + $1))
    while kill -0 -- "$2" 2>/dev/null; do
        ((SECONDS < limit)) || return 1
        sleep 0.2
    done
}

# stop_run: end everything the command started, before the scratch directory
# goes. The command gets its signal again after five seconds, because one that
# reaches a forked child before its exec lands in the handler it inherited from
# this shell, and is lost.
#
# THE LIMIT ON THE COMMAND IS LONG BECAUSE IT COVERS A SESSION'S TEARDOWN: killing
# a session halfway through that leaves the VM up and the machine-wide slot taken,
# so only a process that is stuck should ever reach it. The harness ends what a
# session's command started within five seconds, and what is left in this group
# gets the same: TERM whichever signal began this, because a job a shell script
# started in the background ignores INT, and then KILL.
# FS_EXT4_TEST_STOP_SECONDS changes the limit.
stop_run() {
    gone_within 5 "$CHILD_PID" || kill -s "$STOP_SIGNAL" "$CHILD_PID" 2>/dev/null || true
    gone_within "${FS_EXT4_TEST_STOP_SECONDS:-300}" "$CHILD_PID" || true
    gone_within 0 "-$CHILD_PID" && return 0
    kill -s TERM -- "-$CHILD_PID" 2>/dev/null || true
    gone_within 5 "-$CHILD_PID" && return 0
    echo "test.sh: killing what TERM did not stop" >&2
    kill -s KILL -- "-$CHILD_PID" 2>/dev/null || true
    gone_within 5 "-$CHILD_PID" || echo "test.sh: process group $CHILD_PID is still running" >&2
}

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
# hand, ../rust-fs-core/scripts/tier.sh) exited with the VM idle and the slot held, and
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
#
# IN THE BACKGROUND, IN A PROCESS GROUP OF ITS OWN. Bash runs a trap only once its
# foreground child has ended, so a foreground command would hear of a signal
# only when the whole suite had finished. The group is what holds everything the
# command started. Job control is also why the command keeps this script's
# standard input, and why Bash 3.2 does not start it with INT ignored.
[[ -z "$STOP_STATUS" ]] || exit "$STOP_STATUS"
set -m
${session[@]+"${session[@]}"} cargo test --features cli "$@" &
CHILD_PID=$!
set +m
# One that landed after the check above and before the command had a name.
if [[ -z "$STOP_STATUS" ]]; then
    set +e
    wait "$CHILD_PID"
    STATUS=$?
    set -e
    [[ -n "$STOP_STATUS" ]] || exit "$STATUS"
fi
stop_run
exit "$STOP_STATUS"
