#!/usr/bin/env bash
# A signal sent to scripts/guest-scratch.sh alone stops its command (#621).
#
# That script binds a directory over the repository's tmp/ in the guest, runs a
# command and undoes the bind. Bash runs a trap only once its foreground child
# has ended, so with the command in the foreground, TERM to this one process did
# nothing until the whole suite had finished.
#
# THE REAL SCRIPT, WITH STAND-INS FOR WHAT IT CALLS. `mount`, `umount` and `mktemp`
# are found first on PATH, so no case needs a VM or root, or can touch a real
# mount or /var/tmp. The stand-in `umount` answers as the real one does: it refuses
# what is not mounted, and what is still in use while the command or its job runs.
# Every call goes in a trace, and each case is judged on the whole trace.
#
# A SURVIVOR IS WHAT THE LIMITS LOOK FOR, and each returns as soon as its condition
# holds, so they cost time only when a case fails. A stand-in outlives every limit
# by minutes: it cannot pass by finishing. Every process a case starts is ended
# on the way out, whichever way the case ends, and only after its command line
# shows it is one of this guard's.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
runner="$REPO/scripts/guest-scratch.sh"

START_LIMIT=60
STOP_LIMIT=60

fails=0
note() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

real_mktemp="$(command -v mktemp)"
mkdir -p "$REPO/tmp"
work="$(mktemp -d "$REPO/tmp/guest-scratch-term.XXXXXX")" || exit 1
state=""
repo=""
running_script=""

# A zombie has gone, whatever `kill -0` says of it.
running() {
    local stat
    stat="$(ps -p "$1" -o stat= 2>/dev/null | tr -d ' ')"
    [[ -n "$stat" && "$stat" != Z* ]]
}

await_file() {
    local limit=$((SECONDS + START_LIMIT))
    until [[ -e "$1" ]]; do
        ((SECONDS < limit)) || return 1
        sleep 0.05
    done
}

await_gone() {
    local limit=$((SECONDS + STOP_LIMIT))
    while running "$1"; do
        ((SECONDS < limit)) || return 1
        sleep 0.1
    done
}

ours() { ps -ww -p "$1" -o command= 2>/dev/null | grep -qF "$work"; }

# What is left of a case, the script last: it unmounts once what it started has
# gone, so it gets the chance.
reap() {
    local name pid
    for name in command job; do
        [[ -n "$state" && -s "$state/$name.pid" ]] || continue
        pid="$(cat "$state/$name.pid")"
        if ours "$pid"; then kill -s KILL "$pid" 2>/dev/null; fi
    done
    [[ -n "$running_script" ]] || return 0
    await_gone "$running_script" || true
    if ours "$running_script"; then kill -s KILL "$running_script" 2>/dev/null; fi
    return 0
}

leave() {
    reap
    case "$work" in
        "$REPO"/tmp/guest-scratch-term.*) [[ ! -d "$work" ]] || rm -r "$work" ;;
    esac
}
trap leave EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$work/bin"
cat > "$work/bin/mktemp" <<'MKTEMP'
#!/bin/sh
# Stands in for mktemp: the one call the script makes, in this case's sandbox.
[ "$#" = 2 ] && [ "$1" = -d ] && [ "$2" = /var/tmp/fs-ext4-tests.XXXXXX ] || exit 85
: > "$STANDIN_STATE/making-scratch"
[ -z "${STANDIN_MKTEMP_DELAY:-}" ] || sleep "$STANDIN_MKTEMP_DELAY"
directory="$("$STANDIN_REAL_MKTEMP" -d "$STANDIN_STATE/local.XXXXXX")" || exit 86
printf '%s\n' "$directory" > "$STANDIN_STATE/scratch"
printf '%s\n' "$directory"
MKTEMP
cat > "$work/bin/mount" <<'MOUNT'
#!/bin/sh
# Stands in for mount: the one call the script makes.
if [ "$#" != 3 ] || [ "$1" != --bind ] || [ "$2" != "$(cat "$STANDIN_STATE/scratch")" ] ||
    [ "$3" != "$STANDIN_REPO/tmp" ]; then
    echo "mount called with: $*" >> "$STANDIN_STATE/trace"
    exit 82
fi
: > "$STANDIN_STATE/mounting"
[ -z "${STANDIN_MOUNT_DELAY:-}" ] || sleep "$STANDIN_MOUNT_DELAY"
if [ "${STANDIN_MOUNT_STATUS:-0}" != 0 ]; then
    echo mount-failed >> "$STANDIN_STATE/trace"
    exit "$STANDIN_MOUNT_STATUS"
fi
echo mount >> "$STANDIN_STATE/trace"
MOUNT
cat > "$work/bin/umount" <<'UMOUNT'
#!/bin/sh
# Stands in for umount: it refuses what is not mounted, and what is still in use.
if [ "$#" != 1 ] || [ "$1" != "$STANDIN_REPO/tmp" ]; then
    echo "umount called with: $*" >> "$STANDIN_STATE/trace"
    exit 84
fi
if ! grep -qx mount "$STANDIN_STATE/trace"; then
    echo unmount-of-nothing >> "$STANDIN_STATE/trace"
    exit 32
fi
for name in command job; do
    [ -s "$STANDIN_STATE/$name.pid" ] || continue
    stat="$(ps -p "$(cat "$STANDIN_STATE/$name.pid")" -o stat= 2>/dev/null | tr -d ' ')"
    case "$stat" in
        '' | Z*) ;;
        *)
            echo "unmount-while-$name-runs" >> "$STANDIN_STATE/trace"
            exit 32
            ;;
    esac
done
: > "$STANDIN_STATE/unmounting"
[ -z "${STANDIN_UMOUNT_DELAY:-}" ] || sleep "$STANDIN_UMOUNT_DELAY"
if [ "${STANDIN_UMOUNT_STATUS:-0}" != 0 ]; then
    echo unmount-failed >> "$STANDIN_STATE/trace"
    exit "$STANDIN_UMOUNT_STATUS"
fi
echo unmount >> "$STANDIN_STATE/trace"
UMOUNT
cat > "$work/bin/standin" <<'STANDIN'
#!/bin/sh
# Stands in for the command: says what it was given, and starts a job of its own.
echo "$$" > "$STANDIN_STATE/command.pid"
{
    printf 'command'
    printf ' %s|' "$@"
    printf '\n'
} >> "$STANDIN_STATE/trace"
case "${STANDIN_COMMAND:-}" in
    finishes)
        cat > "$STANDIN_STATE/stdin"
        exit "${STANDIN_STATUS:-0}"
        ;;
    ignores)
        trap '' HUP INT TERM
        ;;
    keeps-an-image)
        printf kept > "$(cat "$STANDIN_STATE/scratch")/image"
        ;;
    tidies-up)
        # As a command with its own scratch does: TERM waits for what it is
        # doing, and a signal to its whole process group would cut that short.
        trap ':' TERM
        : > "$STANDIN_STATE/tidying"
        sh -c 'sleep 3; echo tidied >> "$STANDIN_STATE/trace"'
        exit 5
        ;;
esac
job "${STANDIN_JOB:-}" &
echo "$!" > "$STANDIN_STATE/job.pid"
wait
STANDIN
cat > "$work/bin/job" <<'JOB'
#!/bin/sh
# Stands in for what a test leaves running.
[ "${1:-}" != ignores ] || trap '' HUP INT TERM
n=0
while [ "$n" -lt 300 ]; do
    sleep 1
    n=$((n + 1))
done
JOB
cat > "$work/bin/default-signals" <<'DEFAULTS'
#!/usr/bin/env python3
import os
import signal
import sys

for number in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
    signal.signal(number, signal.SIG_DFL)
os.execv(sys.argv[1], sys.argv[1:])
DEFAULTS
chmod +x "$work/bin/"*

new_case() {
    state="$work/$1/state"
    repo="$work/$1/repo"
    running_script=""
    mkdir -p "$state" "$repo"
    : > "$state/trace"
}

# start [VAR=value...]: the script, in the background, from this case's repository,
# with these signals back at their defaults. A signal ignored on entry to a shell
# cannot be trapped there, and a background job has INT ignored, so otherwise this
# guard's answer would depend on how it was itself started. "$BASH" so the script
# runs under the Bash running this guard.
start() {
    (
        cd "$repo" || exit 1
        exec env -u FS_EXT4_GUEST_STOP_SECONDS \
            PATH="$work/bin:$PATH" FLTH_GUEST=1 STANDIN_STATE="$state" STANDIN_REPO="$repo" \
            STANDIN_REAL_MKTEMP="$real_mktemp" "$@" \
            default-signals "$BASH" "$runner" "$work/bin/standin" 'with spaces' two
    ) < "${input:-/dev/null}" > "$state/out" 2>&1 &
    running_script=$!
}

# The trace a case that reached its command and was stopped leaves behind.
ran="$(printf 'mount\ncommand with spaces| two|\nunmount')"

# judge LABEL MARKER SIGNAL STATUS TRACE: once MARKER exists, signal the script's
# process alone, then hold it to stopping with STATUS and leaving exactly TRACE,
# with its command and that command's job gone. The checks run before reap, which
# would hide a survivor. keeps_scratch says the scratch directory should stay.
judge() {
    local label="$1" marker="$2" signal="$3" want="$4" trace="$5" status name pid scratch
    if ! await_file "$state/$marker"; then
        note "$label: the case never reached $marker: $(tail -n 3 "$state/out" | tr '\n' ' ')"
        reap
        wait "$running_script" 2>/dev/null
        return 1
    fi
    kill -s "$signal" "$running_script"
    if ! await_gone "$running_script"; then
        note "$label: the script is still running $STOP_LIMIT seconds after $signal"
        reap
        wait "$running_script" 2>/dev/null
        return 1
    fi
    wait "$running_script" 2>/dev/null
    status=$?
    [[ "$status" -eq "$want" ]] || note "$label: the script exited $status, not $want"
    for name in command job; do
        [[ -s "$state/$name.pid" ]] || continue
        pid="$(cat "$state/$name.pid")"
        await_gone "$pid" || note "$label: the script exited and left the $name running"
    done
    [[ "$(cat "$state/trace")" == "$trace" ]] ||
        note "$label: the trace was: $(tr '\n' ';' < "$state/trace")"
    scratch="$(cat "$state/scratch" 2>/dev/null)"
    if [[ -z "${keeps_scratch:-}" && -n "$scratch" && -d "$scratch" ]]; then
        note "$label: the scratch directory is still there"
    fi
    reap
}

# --- The signal stops the command and what it started. ----------------------
new_case term
start
judge "TERM" job.pid TERM 143 "$ran"

new_case hup
start
judge "HUP" job.pid HUP 129 "$ran"

new_case int
start
judge "INT" job.pid INT 130 "$ran"

# --- What does not stop for TERM is ended all the same. ---------------------
new_case stubborn-job
start STANDIN_JOB=ignores
judge "a job that ignores TERM" job.pid TERM 143 "$ran"

new_case stubborn-command
start STANDIN_COMMAND=ignores FS_EXT4_GUEST_STOP_SECONDS=2
judge "a command that ignores TERM" job.pid TERM 143 "$ran"

# --- The command finishes what it is doing, with the mount still there. -----
new_case tidies
start STANDIN_COMMAND=tidies-up
judge "a command that tidies up" tidying TERM 143 \
    "$(printf 'mount\ncommand with spaces| two|\ntidied\nunmount')"

# --- The scratch directory is handled as it always was. ---------------------
new_case image
start STANDIN_COMMAND=keeps-an-image
keeps_scratch=1 judge "TERM with an image in scratch" job.pid TERM 143 "$ran"
[[ "$(cat "$(cat "$state/scratch")/image" 2>/dev/null)" == kept ]] ||
    note "TERM with an image in scratch: the image was removed"
grep -q 'scratch retained at' "$state/out" ||
    note "TERM with an image in scratch: the retained directory was not reported"

new_case stuck-mount
start STANDIN_UMOUNT_STATUS=71
keeps_scratch=1 judge "TERM and an unmount that fails" job.pid TERM 143 \
    "$(printf 'mount\ncommand with spaces| two|\nunmount-failed')"
[[ -d "$(cat "$state/scratch")" ]] ||
    note "TERM and an unmount that fails: scratch was removed while still mounted"
grep -q 'could not unmount' "$state/out" ||
    note "TERM and an unmount that fails: the failure was not explained"

# --- A signal before the command starts means it never starts. --------------
new_case during-scratch
start STANDIN_MKTEMP_DELAY=2
judge "TERM while scratch is being made" making-scratch TERM 143 ""

new_case during-mount
start STANDIN_MOUNT_DELAY=2
judge "TERM while mounting" mounting TERM 143 "$(printf 'mount\nunmount')"

# A mount that failed is not ours to undo, whatever else has happened.
new_case during-failed-mount
start STANDIN_MOUNT_DELAY=2 STANDIN_MOUNT_STATUS=77
judge "TERM while a mount fails" mounting TERM 143 mount-failed

# --- A signal while unmounting does not stop the cleaning up. ---------------
new_case during-unmount
printf 'from the caller\n' > "$state/input"
input="$state/input" start STANDIN_COMMAND=finishes STANDIN_STATUS=7 STANDIN_UMOUNT_DELAY=2
judge "TERM while unmounting" unmounting TERM 143 \
    "$(printf 'mount\ncommand with spaces| two|\nunmount')"

# --- An ordinary run is what it was. ----------------------------------------
new_case ordinary
printf 'from the caller\n' > "$state/input"
input="$state/input" start STANDIN_COMMAND=finishes STANDIN_STATUS=7
wait "$running_script"
status=$?
[[ "$status" -eq 7 ]] || note "an ordinary run: the command's status 7 came back as $status"
[[ "$(cat "$state/stdin" 2>/dev/null)" == "from the caller" ]] ||
    note "an ordinary run: the command did not get the script's standard input"
[[ "$(cat "$state/trace")" == "$ran" ]] ||
    note "an ordinary run: the trace was: $(tr '\n' ';' < "$state/trace")"
[[ -n "$(cat "$state/scratch" 2>/dev/null)" && ! -d "$(cat "$state/scratch")" ]] ||
    note "an ordinary run: the scratch directory was not removed"
reap

if [[ "$fails" -gt 0 ]]; then
    exit 1
fi
echo "PASS  a signal to scripts/guest-scratch.sh alone stops its command and unmounts"
