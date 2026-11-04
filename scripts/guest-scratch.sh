#!/usr/bin/env bash
set -euo pipefail

[ "${FLTH_GUEST:-}" = 1 ] ||
    { echo "guest-scratch.sh runs INSIDE the harness VM." >&2; exit 1; }
[ "$#" -gt 0 ] || { echo "guest-scratch.sh needs a command." >&2; exit 2; }

# A SIGNAL STOPS THE RUN, AND ONLY RECORDS THAT IT DID. Exiting from the trap would
# leave a mount behind: a signal during `mount` runs the trap once `mount` has
# returned, before `mounted=1` can say there is something to undo. Until the command
# exists there is nothing to pass the signal to, and the command is not started.
# Once it does, the signal goes to the command's process alone: scripts/test.sh
# passes it on and removes its own scratch, and a signal to its whole process group
# would cut that short. stop_command deals with the rest of the group.
child=""
stop_signal=""
stop_status=""
note_signal() {
    stop_signal="$1"
    stop_status=$((128 + $2))
    if [ -n "$child" ]; then
        kill -s "$1" "$child" 2>/dev/null || true
    fi
}
trap 'note_signal HUP 1' HUP
trap 'note_signal INT 2' INT
trap 'note_signal TERM 15' TERM

repo="$(pwd -P)"
scratch=""
mounted=0

cleanup() {
    status=$?
    trap - EXIT
    if [ "$mounted" = 1 ] && ! umount "$repo/tmp"; then
        echo "guest-scratch.sh: could not unmount $repo/tmp; scratch retained at $scratch" >&2
        [ "$status" -ne 0 ] || status=1
    elif [ -n "$scratch" ]; then
        rmdir "$scratch" 2>/dev/null ||
            echo "guest-scratch.sh: scratch retained at $scratch" >&2
    fi
    [ -z "$stop_status" ] || status="$stop_status"
    exit "$status"
}
trap cleanup EXIT

# gone_within SECONDS TARGET: kill's TARGET, a process or a -group, has no process
# left in it, waiting up to SECONDS.
gone_within() {
    local limit=$((SECONDS + $1))
    while kill -0 -- "$2" 2>/dev/null; do
        [ "$SECONDS" -lt "$limit" ] || return 1
        sleep 0.2
    done
}

# stop_command: end everything the command started, before the unmount, which
# fails while anything in the mount is still in use. The command gets its signal
# again after five seconds, because one that reaches a forked child before its exec
# lands in the handler it inherited from this shell, and is lost.
#
# THE LIMIT ON THE COMMAND IS LONG BECAUSE scripts/test.sh HAS ONE OF ITS OWN: 300
# seconds for what it runs, ten for what that left, then its scratch directory to
# remove. Killing it sooner would leave its command running with nothing to stop
# it, so only a process that is stuck should ever reach this. What is left in the
# group then gets TERM whichever signal began this, because a job a shell script
# started in the background ignores INT, and then KILL.
# FS_EXT4_GUEST_STOP_SECONDS changes the limit.
stop_command() {
    gone_within 5 "$child" || kill -s "$stop_signal" "$child" 2>/dev/null || true
    gone_within "${FS_EXT4_GUEST_STOP_SECONDS:-360}" "$child" || true
    gone_within 0 "-$child" && return 0
    kill -s TERM -- "-$child" 2>/dev/null || true
    gone_within 5 "-$child" && return 0
    echo "guest-scratch.sh: killing what TERM did not stop" >&2
    kill -s KILL -- "-$child" 2>/dev/null || true
    gone_within 5 "-$child" || echo "guest-scratch.sh: process group $child is still running" >&2
}

scratch="$(mktemp -d /var/tmp/fs-ext4-tests.XXXXXX)"
[ -z "$stop_status" ] || exit "$stop_status"
mkdir -p "$repo/tmp"
mount --bind "$scratch" "$repo/tmp"
mounted=1
[ -z "$stop_status" ] || exit "$stop_status"

# IN THE BACKGROUND, IN A PROCESS GROUP OF ITS OWN. Bash runs a trap only once its
# foreground child has ended, so a foreground command would hear of a signal only
# when the whole suite had finished. The group is what holds everything the command
# started. Job control is also why the command keeps this script's standard input,
# and why Bash 3.2 does not start it with INT ignored.
set -m
"$@" &
child=$!
set +m
# One that landed after the check above and before the command had a name.
if [ -z "$stop_status" ]; then
    set +e
    wait "$child"
    status=$?
    set -e
    [ -n "$stop_status" ] || exit "$status"
fi
stop_command
exit "$stop_status"
