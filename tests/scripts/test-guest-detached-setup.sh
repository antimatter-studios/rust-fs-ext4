#!/usr/bin/env bash
# A cold guest's live setup outlives an SSH stall (scripts/ci-guest-detached.sh).
#
# Run 45282138545 (job 138069722608) lost its cold setup to "Timeout, server
# 127.0.0.1 not responding." The setup ran as one foreground SSH command, the
# runner stalled for longer than SSH's keepalive allows, and SSH ended with 255
# while the guest was still installing rustup's toolchain.
#
# THE REAL HELPER, WITH STAND-INS FOR WHAT IT CALLS. `ssh` runs the remote
# command here, in a scratch "guest" directory, and fails with 255 on the calls
# a case names: either before running the command (the guest unreachable) or
# after it (the connection lost once the guest had acted). `systemd-run` starts
# the command in the background, detached, as the guest's systemd would. No case
# needs a VM, root or the network.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
helper="$REPO/scripts/ci-guest-detached.sh"
scratch="$(mktemp -d)"
fails=0
note() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }
# A stand-in unit and everything under it, by pid, never by pattern.
kill_tree() {
    local child
    for child in $(pgrep -P "$1"); do kill_tree "$child"; done
    kill "$1" 2>/dev/null
}
cleanup() {
    local pid
    while read -r pid; do
        if ps -o command= -p "$pid" 2>/dev/null | grep -q "$scratch"; then kill_tree "$pid"; fi
    done < <(cat "$scratch"/*/units 2>/dev/null)
    kill "${qemu:-}" 2>/dev/null
    rm -rf "$scratch"
}
trap cleanup EXIT

mkdir -p "$scratch/bin"
cat >"$scratch/bin/ssh" <<'EOF'
#!/usr/bin/env bash
# The last argument is the remote command; the others are SSH's options.
# SSH_OUTAGE="<from> <to>", in seconds since the case began: a call made in
# that window fails at once, and a call still running when it opens loses
# its remote command and fails, as a keepalive timeout ends a session.
command="${!#}"
n=$(( $(cat "$CASE/calls" 2>/dev/null || echo 0) + 1 ))
echo "$n" >"$CASE/calls"
read -r from to <<<"${SSH_OUTAGE:-0 0}"
down() { local t=$(( $(date +%s) - $(cat "$CASE/began") )); [ "$t" -ge "$from" ] && [ "$t" -lt "$to" ]; }
down && exit 255
bash -c "$command" &
remote=$!
while kill -0 "$remote" 2>/dev/null; do
    if down; then pkill -P "$remote" 2>/dev/null; kill "$remote" 2>/dev/null; exit 255; fi
    sleep 0.2
done
status=0
wait "$remote" || status=$?
for lost in ${SSH_LOST:-}; do [ "$n" = "$lost" ] && exit 255; done
exit "$status"
EOF
cat >"$scratch/bin/systemd-run" <<'EOF'
#!/usr/bin/env bash
while [ "${1:-}" != "${1#--}" ]; do shift; done
echo start >>"$CASE/starts"
"$@" </dev/null >/dev/null 2>&1 &
echo $! >>"$CASE/units"
EOF
chmod +x "$scratch/bin/ssh" "$scratch/bin/systemd-run"

sleep 600 &
qemu=$!
dead_qemu="$(bash -c 'echo $$')"

# run <case> <deadline> <guest script> [env...]: the helper's status, its
# output in <case>/out, its local log in <case>/log.
run() {
    local case="$scratch/$1" deadline="$2" script="$3"
    shift 3
    mkdir -p "$case/guest"
    date +%s >"$case/began"
    env PATH="$scratch/bin:$PATH" CASE="$case" CI_GUEST_DIR="$case/guest" \
        CI_GUEST_POLL_SECONDS=1 CI_GUEST_UNREACHABLE_SECONDS=30 "$@" \
        bash "$helper" setup "$deadline" "$case/log" "${QEMU:-$qemu}" -- -p 1 root@127.0.0.1 \
        <<<"$script" >"$case/out" 2>&1
}
expected=$'one\ntwo\nthree\n'
steps='echo one; sleep 1; echo two; sleep 2; echo three'

# The runner stalls for three seconds while the setup runs: every SSH call in
# that window fails, and one still open is cut. The setup still finishes and
# its log is whole.
run stall 60 "$steps" SSH_OUTAGE='2 5'
status=$?
[ "$status" = 0 ] || note "stall: status $status, not 0: $(tail -3 "$scratch/stall/out")"
[ "$(cat "$scratch/stall/log")" = "${expected%$'\n'}" ] || note "stall: log is $(cat "$scratch/stall/log")"
grep -q 'SSH to the guest failed (255)' "$scratch/stall/out" || note 'stall: the retries were not reported'

# The connection is lost just after the start ran: the retried start starts
# nothing more.
run restart 60 "$steps" SSH_LOST=2
status=$?
[ "$status" = 0 ] || note "restart: status $status, not 0"
[ "$(wc -l <"$scratch/restart/starts" 2>/dev/null | tr -d ' ')" = 1 ] || note 'restart: the setup did not start exactly once'

# The setup's own failure is the helper's status.
run failed 60 'echo broken; exit 7'
status=$?
[ "$status" = 7 ] || note "failed: status $status, not 7"
grep -q broken "$scratch/failed/log" || note 'failed: the log lost the output'

# A guest that stays unreachable fails as SSH does, naming why.
run unreachable 60 'sleep 20' SSH_OUTAGE='1 60' CI_GUEST_UNREACHABLE_SECONDS=3
status=$?
[ "$status" = 255 ] || note "unreachable: status $status, not 255"
grep -q 'guest unreachable over SSH for 3s' "$scratch/unreachable/out" || note 'unreachable: no reason given'

# A setup that outlives its deadline fails with 124.
run deadline 2 'sleep 20'
status=$?
[ "$status" = 124 ] || note "deadline: status $status, not 124"
grep -q 'did not finish in the guest within 2s' "$scratch/deadline/out" || note 'deadline: no reason given'

# QEMU gone ends the wait at once.
QEMU="$dead_qemu" run qemu 60 'sleep 20'
status=$?
[ "$status" = 1 ] || note "qemu: status $status, not 1"
grep -q "QEMU (pid $dead_qemu) exited" "$scratch/qemu/out" || note 'qemu: no reason given'

if [ "$fails" -ne 0 ]; then exit 1; fi
echo 'PASS  a detached guest setup outlives SSH stalls and fails only for a stated reason'
