#!/usr/bin/env bash
# ci-guest-detached.sh -- run a long guest script detached from SSH, and wait
# for it with short SSH calls that a stalled host or guest does not end.
#
#   ci-guest-detached.sh <name> <deadline-seconds> <local-log> <qemu-pid> -- <ssh args...> <<'GUEST'
#   ...the script to run in the guest, as root...
#   GUEST
#
# WHY. A cold guest's live setup (scripts/ci-qemu-benchmark.sh serve) runs
# for twenty minutes or more under TCG. Run as one foreground SSH command, a
# host stall longer than ServerAliveInterval x ServerAliveCountMax kills the
# SSH session with status 255 and the setup with it, although the guest was
# still installing: run 45282138545, job 138069722608, "Timeout, server
# 127.0.0.1 not responding." while rustup installed the toolchain, after the
# job's own heartbeat had paused 100 s with the runner's CPU pressure at 79%.
#
# HOW. The script is copied into the guest and started there as a transient
# systemd unit, so it does not belong to any SSH session. Each poll then
# fetches the new part of its log and its exit status with one short SSH
# call. A failed poll is waited out, not fatal, until the guest has been
# unreachable for CI_GUEST_UNREACHABLE_SECONDS in a row.
#
# IT FAILS, NAMING WHY, when QEMU exits, when the guest stays unreachable,
# when the deadline passes (124), or with the script's own exit status.
set -euo pipefail

name="${1:?unit name required}"
deadline="${2:?deadline seconds required}"
log="${3:?local log path required}"
qemu_pid="${4:?QEMU pid required}"
shift 4
[ "${1:-}" = -- ] || { echo 'usage: ci-guest-detached.sh <name> <deadline> <log> <qemu-pid> -- <ssh args...>' >&2; exit 2; }
shift
ssh_args=("$@")
[ "${#ssh_args[@]}" -gt 0 ] || { echo 'ssh arguments required' >&2; exit 2; }
case "$name" in *[!a-z0-9-]*|'') echo "unit name must be [a-z0-9-]: $name" >&2; exit 2 ;; esac
case "$deadline" in *[!0-9]*|'') echo "deadline must be whole seconds: $deadline" >&2; exit 2 ;; esac

poll_seconds="${CI_GUEST_POLL_SECONDS:-15}"
unreachable_budget="${CI_GUEST_UNREACHABLE_SECONDS:-900}"
guest_dir="${CI_GUEST_DIR:-/var/tmp}"
base="$guest_dir/$name"

: >"$log"
alive() {
    kill -0 "$qemu_pid" 2>/dev/null || { echo "QEMU (pid $qemu_pid) exited while $name ran in the guest" >&2; exit 1; }
}
# One SSH call, retried while the guest is unreachable. Its stdout goes to
# the caller; nothing is retried that the guest has already acted on, because
# every command given here is idempotent.
guest() {
    local started status
    started="$(date +%s)"
    while :; do
        alive
        status=0
        ssh "${ssh_args[@]}" "$1" || status=$?
        [ "$status" = 255 ] || return "$status"
        if [ "$(( $(date +%s) - started ))" -ge "$unreachable_budget" ]; then
            echo "guest unreachable over SSH for ${unreachable_budget}s while $name ran; giving up" >&2
            exit 255
        fi
        printf '[%s] SSH to the guest failed (255); %s keeps running there, retrying in %ss\n' \
            "$(date -u +%FT%TZ)" "$name" "$poll_seconds" >&2
        sleep "$poll_seconds"
    done
}

script="$(cat)"
[ -n "$script" ] || { echo 'no guest script on stdin' >&2; exit 2; }
printf -v upload 'mkdir -p %q && printf %%s %q >%q.sh' "$guest_dir" "$script" "$base"
guest "$upload" </dev/null
# Started once: a retried start finds the marker and starts nothing more.
printf -v start 'if [ ! -e %q.started ]; then rm -f %q.log %q.status; touch %q.started; systemd-run --unit=%q --collect --quiet /bin/bash -c %q; fi' \
    "$base" "$base" "$base" "$base" "$name" \
    "bash $base.sh >$base.log 2>&1; echo \$? >$base.status.partial; mv $base.status.partial $base.status"
guest "$start" </dev/null

# Each poll reads into files, not command substitutions, so a guest() that
# gives up ends this script rather than a subshell.
chunk="$log.chunk"
status_file="$log.status"
trap 'rm -f "$chunk" "$status_file"' EXIT
fetch_log() {
    guest "tail -c +$(( $(wc -c <"$log") + 1 )) $base.log 2>/dev/null || true" </dev/null >"$chunk"
    if [ -s "$chunk" ]; then tee -a "$log" <"$chunk"; fi
}
started="$(date +%s)"
while :; do
    fetch_log
    guest "cat $base.status 2>/dev/null || true" </dev/null >"$status_file"
    status="$(tr -d '[:space:]' <"$status_file")"
    if [ -n "$status" ]; then
        fetch_log
        case "$status" in *[!0-9]*) echo "unreadable exit status from $name: $status" >&2; exit 1 ;; esac
        [ "$status" = 0 ] || echo "$name failed in the guest with status $status" >&2
        exit "$status"
    fi
    if [ "$(( $(date +%s) - started ))" -ge "$deadline" ]; then
        echo "$name did not finish in the guest within ${deadline}s" >&2
        exit 124
    fi
    sleep "$poll_seconds"
done
