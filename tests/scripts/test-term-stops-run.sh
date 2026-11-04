#!/usr/bin/env bash
# A signal sent to scripts/test.sh alone stops the run (#618).
#
# The wrapper is what a tier, CI and a person at a terminal all start, and it
# traps TERM, so signalling it has to stop what it started. Bash runs a trap only
# once its foreground child has ended: with the command in the foreground, TERM to
# this one process did nothing until the whole suite had finished.
#
# DRIVEN THROUGH A BYTE-FOR-BYTE COPY of the wrapper in a sandbox, because the
# wrapper finds the harness beside its own repository, and a copy is how each
# case chooses what it finds there: nothing, a session that runs the command
# as it is (the guest, and a host that cannot run a VM), or a session that
# keeps the command in a process group of its own and tears down afterwards (a
# host that can). `cargo` and the session are stand-ins, so no case needs a VM or
# can touch one. fs-linux-test-harness holds the real session to its own promises.
#
# A SURVIVOR IS WHAT THE LIMITS LOOK FOR, and each returns as soon as its condition
# holds, so they cost time only when a case fails. A stand-in outlives every limit
# by minutes: it cannot pass by finishing. Every process a case starts is ended
# on the way out, whichever way the case ends, and only after its command line
# shows it is one of this guard's.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

START_LIMIT=60
STOP_LIMIT=30

fails=0
note() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

mkdir -p "$REPO/tmp"
work="$(mktemp -d "$REPO/tmp/term-stops-run.XXXXXX")" || exit 1
tree=""
wrapper=""

await_file() {
    local limit=$((SECONDS + START_LIMIT))
    until [[ -s "$1" || -e "$1" && "${2:-}" == empty-is-fine ]]; do
        ((SECONDS < limit)) || return 1
        sleep 0.05
    done
}

# A zombie has gone, whatever `kill -0` says of it.
running() {
    local stat
    stat="$(ps -p "$1" -o stat= 2>/dev/null | tr -d ' ')"
    [[ -n "$stat" && "$stat" != Z* ]]
}

await_gone() {
    local limit=$((SECONDS + STOP_LIMIT))
    while running "$1"; do
        ((SECONDS < limit)) || return 1
        sleep 0.1
    done
}

ours() { ps -ww -p "$1" -o command= 2>/dev/null | grep -qF "$work"; }

# What is left of a case, the wrapper last: it removes its own scratch directory
# once what it started has gone, so it gets the chance.
reap() {
    local name pid
    for name in cargo job session; do
        [[ -n "$tree" && -s "$tree/state/$name.pid" ]] || continue
        pid="$(cat "$tree/state/$name.pid")"
        if ours "$pid"; then kill -s KILL "$pid" 2>/dev/null; fi
    done
    [[ -n "$wrapper" ]] || return 0
    await_gone "$wrapper" || true
    if ours "$wrapper"; then kill -s KILL "$wrapper" 2>/dev/null; fi
    return 0
}

leave() {
    reap
    case "$work" in
        "$REPO"/tmp/term-stops-run.*) [[ ! -d "$work" ]] || rm -r "$work" ;;
    esac
}
trap leave EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$work/bin"
cat > "$work/bin/cargo" <<'CARGO'
#!/bin/sh
# Stands in for cargo: says what it was given, and starts a job of its own.
echo "$$" > "$STANDIN_STATE/cargo.pid"
printf '%s\n' "$*" > "$STANDIN_STATE/args"
printf '%s\n' "$TMPDIR" > "$STANDIN_STATE/scratch"
: > "$TMPDIR/witness"
case "${STANDIN_CARGO:-}" in
    finishes)
        cat > "$STANDIN_STATE/stdin"
        exit "${STANDIN_STATUS:-0}"
        ;;
    ignores)
        trap '' HUP INT TERM
        ;;
esac
job "${STANDIN_JOB:-}" &
echo "$!" > "$STANDIN_STATE/job.pid"
wait
CARGO
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
cat > "$work/vm.sh" <<'SESSION'
#!/usr/bin/env bash
# Stands in for `vm.sh session`, as the pinned harness behaves: no session where
# no VM can run, and otherwise a signal goes to the command's process group, and
# teardown follows once the command has gone.
[[ "${1:-}" == session ]] || exit 99
shift
[[ "$STANDIN_SESSION" != exec ]] || exec "$@"
echo "$$" > "$STANDIN_STATE/session.pid"
child=""
signalled=0
trap 'signalled=143; [[ -z "$child" ]] || kill -s TERM -- "-$child" 2>/dev/null || true' TERM
set -m
"$@" &
child=$!
set +m
while :; do
    wait "$child"
    status=$?
    kill -0 "$child" 2>/dev/null || break
done
[[ ! -d "$TMPDIR" ]] || : > "$STANDIN_STATE/scratch-seen-in-teardown"
: > "$STANDIN_STATE/teardown-started"
sh -c 'sleep 3; : > "$STANDIN_STATE/teardown-finished"'
[[ "$signalled" == 0 ]] || status="$signalled"
exit "$status"
SESSION
cat > "$work/bin/default-signals" <<'DEFAULTS'
#!/usr/bin/env python3
import os
import signal
import sys

for number in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
    signal.signal(number, signal.SIG_DFL)
os.execv(sys.argv[1], sys.argv[1:])
DEFAULTS
chmod +x "$work/bin/cargo" "$work/bin/job" "$work/bin/default-signals" "$work/vm.sh"

# new_tree NAME SESSION: a checkout holding the real wrapper, and the session it
# will find beside it. A session of `none` is a checkout with no harness sibling.
new_tree() {
    tree="$work/$1"
    wrapper=""
    mkdir -p "$tree/repo/scripts" "$tree/state"
    cp "$REPO/scripts/test.sh" "$tree/repo/scripts/test.sh"
    session_shape="$2"
    if [[ "$session_shape" != none ]]; then
        mkdir -p "$tree/fs-linux-test-harness/scripts"
        cp "$work/vm.sh" "$tree/fs-linux-test-harness/scripts/vm.sh"
    fi
}

# start [VAR=value...]: the wrapper, in the background, with these signals back at
# their defaults. A signal ignored on entry to a shell cannot be trapped there, and
# a background job has INT ignored, so otherwise this guard's answer would depend on
# how it was itself started, and Bash has no way to undo that.
start() {
    env -u FLTH_GUEST -u FS_EXT4_TEST_TMPDIR \
        PATH="$work/bin:$PATH" STANDIN_STATE="$tree/state" STANDIN_SESSION="$session_shape" "$@" \
        default-signals "$tree/repo/scripts/test.sh" --locked < "${input:-/dev/null}" > "$tree/out" 2>&1 &
    wrapper=$!
}

# stop LABEL SIGNAL STATUS: signal the wrapper's process alone, and hold everything
# to having stopped. The checks run before reap, which would hide a survivor.
stop() {
    local label="$1" signal="$2" want="$3" state="$tree/state" pid="" scratch="" status
    if ! await_file "$state/job.pid"; then
        note "$label: the stand-in cargo did not start its job: $(tail -n 3 "$tree/out" | tr '\n' ' ')"
        reap
        wait "$wrapper" 2>/dev/null
        return 1
    fi
    pid="$(cat "$state/job.pid")"
    scratch="$(cat "$state/scratch")"
    kill -s "$signal" "$wrapper"
    if ! await_gone "$wrapper"; then
        note "$label: the wrapper is still running $STOP_LIMIT seconds after $signal"
        running "$pid" && note "$label: and so is the command's job"
        reap
        wait "$wrapper" 2>/dev/null
        return 1
    fi
    wait "$wrapper" 2>/dev/null
    status=$?
    [[ "$status" -eq "$want" ]] || note "$label: the wrapper exited $status, not $want"
    await_gone "$pid" || note "$label: the wrapper exited and left the command's job running"
    if [[ -d "$scratch" && -z "${keeps_scratch:-}" ]]; then
        note "$label: the scratch directory is still there: $scratch"
    fi
    reap
}

# --- The signal reaches the whole run, wherever the session put it. ---------
new_tree alone none
start
stop "no harness, TERM" TERM 143

new_tree execs exec
start
stop "a session that runs the command as it is, TERM" TERM 143

new_tree group own-group
start
if stop "a session with a process group of its own, TERM" TERM 143; then
    [[ -e "$tree/state/scratch-seen-in-teardown" ]] ||
        note "the scratch directory was removed before the session's teardown"
    [[ -e "$tree/state/teardown-finished" ]] ||
        note "the wrapper did not wait for the session's teardown"
fi

new_tree hup none
start
stop "no harness, HUP" HUP 129

new_tree int none
start
stop "no harness, INT" INT 130

# --- What does not stop for TERM is ended all the same. ---------------------
new_tree stubborn-job none
start STANDIN_JOB=ignores
stop "a job that ignores TERM" TERM 143

new_tree stubborn-command exec
start STANDIN_CARGO=ignores FS_EXT4_TEST_STOP_SECONDS=2
stop "a command that ignores TERM" TERM 143

# --- A signal during teardown does not cut the teardown short. --------------
new_tree teardown own-group
start STANDIN_CARGO=finishes
if await_file "$tree/state/teardown-started" empty-is-fine; then
    kill -s TERM "$wrapper"
    await_gone "$wrapper" || note "TERM during teardown: the wrapper is still running"
    [[ -e "$tree/state/teardown-finished" ]] || note "TERM during teardown: the teardown was cut short"
else
    note "the session's teardown never started"
fi
reap
wait "$wrapper" 2>/dev/null
status=$?
[[ "$status" -eq 143 ]] || note "TERM during teardown: the wrapper exited $status, not 143"

# --- A signal before the command exists means it never starts. --------------
new_tree startup none
real_mktemp="$(command -v mktemp)"
mkdir -p "$tree/slow"
cat > "$tree/slow/mktemp" <<SLOW
#!/bin/sh
: > "$tree/state/starting"
sleep 1
exec "$real_mktemp" "\$@"
SLOW
chmod +x "$tree/slow/mktemp"
saved_path="$PATH"
PATH="$tree/slow:$PATH"
start
PATH="$saved_path"
if await_file "$tree/state/starting" empty-is-fine; then
    kill -s TERM "$wrapper"
    await_gone "$wrapper" || note "TERM during startup: the wrapper is still running $STOP_LIMIT seconds later"
else
    note "the wrapper never reached its scratch directory's mktemp"
fi
started=0
[[ ! -e "$tree/state/cargo.pid" ]] || started=1
reap
wait "$wrapper" 2>/dev/null
status=$?
[[ "$started" == 0 ]] || note "TERM during startup: the command started anyway"
[[ "$status" -eq 143 ]] || note "TERM during startup: the wrapper exited $status, not 143"
if [[ -n "$(find "$tree/repo/tmp" -mindepth 1 -print 2>/dev/null)" ]]; then
    note "TERM during startup left a scratch directory: $(find "$tree/repo/tmp" -mindepth 1 -print | tr '\n' ' ')"
fi

# --- A directory the caller supplied is still the caller's. -----------------
new_tree exact none
exact="$tree/repo/tmp/exact"
start FS_EXT4_TEST_TMPDIR="$exact"
keeps_scratch=1 stop "a directory the caller supplied, TERM" TERM 143
[[ -e "$exact/witness" ]] || note "a signal removed what was in a directory the caller supplied"

# --- An ordinary run is what it was. ----------------------------------------
for shape in none exec own-group; do
    new_tree "finishes-$shape" "$shape"
    printf 'from the caller\n' > "$tree/input"
    input="$tree/input" start STANDIN_CARGO=finishes STANDIN_STATUS=7
    wait "$wrapper"
    status=$?
    [[ "$status" -eq 7 ]] || note "$shape: the command's status 7 came back as $status"
    args="$(cat "$tree/state/args" 2>/dev/null)"
    [[ "$args" == "test --features cli --locked" ]] || note "$shape: the command was given: $args"
    [[ "$(cat "$tree/state/stdin" 2>/dev/null)" == "from the caller" ]] ||
        note "$shape: the command did not get the wrapper's standard input"
    scratch="$(cat "$tree/state/scratch" 2>/dev/null)"
    [[ -n "$scratch" && ! -e "$scratch" ]] || note "$shape: the scratch directory was not removed: $scratch"
done

if [[ "$fails" -gt 0 ]]; then
    exit 1
fi
echo "PASS  a signal to scripts/test.sh alone stops the run, ends what it started and removes its scratch"
