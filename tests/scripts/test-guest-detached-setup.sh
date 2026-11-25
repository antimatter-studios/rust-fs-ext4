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

cold_setup=$(sed -n "/<<'GUEST'/,/^GUEST\$/p" "$REPO/scripts/ci-qemu-benchmark.sh" | sed '1d;$d')
mkdir -p "$scratch/dns-bin" "$scratch/dns/repo/scripts" "$scratch/dns/rust-fs-core/scripts"
printf 'echo vm-setup\napt-get update -qq\n' >"$scratch/dns/repo/scripts/vm-setup.sh"
printf 'echo toolchain-setup\n' >"$scratch/dns/rust-fs-core/scripts/guest-rust-toolchain.sh"
cat >"$scratch/dns-bin/timeout" <<'EOF'
#!/usr/bin/env bash
shift
exec "$@"
EOF
cat >"$scratch/dns-bin/getent" <<'EOF'
#!/usr/bin/env bash
[[ "$*" == 'ahostsv4 deb.debian.org' ]] || exit 2
case "$DNS_CASE" in
    healthy) exit 0 ;;
    transient) [ -f "$CASE/probed" ] && exit 0; touch "$CASE/probed"; exit 2 ;;
    fallback) [ -s "$CASE/resolver" ] ;;
    unavailable) exit 2 ;;
esac
EOF
cat >"$scratch/dns-bin/ip" <<'EOF'
#!/usr/bin/env bash
echo 'default via 10.0.2.2 dev enp0s3 proto static'
EOF
cat >"$scratch/dns-bin/resolvectl" <<'EOF'
#!/usr/bin/env bash
case "$*" in
    'dns enp0s3 1.1.1.1 8.8.8.8') echo fallback >"$CASE/resolver" ;;
    'dns enp0s3 10.0.2.3') : >"$CASE/resolver"; echo 'restored QEMU DNS' ;;
    'flush-caches') ;;
    status) echo 'resolver status snapshot' ;;
    *) exit 1 ;;
esac
EOF
cat >"$scratch/dns-bin/apt-get" <<'EOF'
#!/usr/bin/env bash
echo "apt-get $*"
[ -z "${APT_CONFIG:-}" ] || cat "$APT_CONFIG"
if [ "$DNS_CASE" = unavailable ]; then
    echo "Temporary failure resolving 'deb.debian.org'" >&2
    exit 100
fi
EOF
printf '#!/usr/bin/env bash\nexit 0\n' >"$scratch/dns-bin/sleep"
chmod +x "$scratch/dns-bin/"*
for scenario in healthy transient fallback unavailable; do
    case_dir="$scratch/dns/$scenario"
    mkdir -p "$case_dir"
    status=0
    output=$(env PATH="$scratch/dns-bin:$PATH" CASE="$case_dir" DNS_CASE="$scenario" TMPDIR="$case_dir" \
        bash -c 'cd() { builtin cd "$(dirname "$CASE")/repo"; }; source /dev/stdin' <<<"$cold_setup" 2>&1) || status=$?
    if [ "$scenario" = unavailable ]; then
        [ "$status" = 100 ] || note "DNS $scenario: status $status, expected 100"
        [[ "$output" == *'restored QEMU DNS'*'resolver status snapshot'* ]] || note "DNS $scenario: missing failure evidence"
        [[ "$output" != *'apt-get install '* ]] || note "DNS $scenario: install ran after failed index update"
    else
        [ "$status" = 0 ] || note "DNS $scenario: status $status, expected 0"
        [[ "$output" == *'apt-get install '*vm-setup*'apt-get update -qq'*toolchain-setup* ]] || note "DNS $scenario: setup did not run in full"
    fi
    if [ "$scenario" = fallback ]; then
        [ -s "$case_dir/resolver" ] || note 'DNS fallback: alternate resolver was not retained after a successful probe'
    else
        [ ! -s "$case_dir/resolver" ] || note "DNS $scenario: unexpectedly retained alternate resolver"
    fi
    [[ "$output" == *'Acquire::Retries "5";'*'Acquire::Retries::Delay::Maximum "15";'*'APT::Update::Error-Mode "any";'* ]] || note "DNS $scenario: missing apt retry or strict update policy"
done

# A fixture-only cold guest keeps the loop and e2fsprogs requirements
# without building the Rust and lwext4 test toolchains.
cat >"$scratch/dns-bin/modprobe" <<'EOF'
#!/usr/bin/env bash
echo "modprobe $*"
test "$*" = loop
EOF
cat >"$scratch/dns-bin/dpkg-query" <<'EOF'
#!/usr/bin/env bash
echo "$E2FSPROGS_VERSION"
EOF
cat >"$scratch/dns-bin/dpkg" <<'EOF'
#!/usr/bin/env bash
[[ "$1" == --compare-versions && "$2" == 1.47.0-2+b2 && "$3" == ge && "$4" == 1.47.0 ]]
EOF
chmod +x "$scratch/dns-bin/"{modprobe,dpkg-query,dpkg}
for version in 1.47.0-2+b2 1.46.6; do
    status=0
    output=$(env PATH="$scratch/dns-bin:$PATH" CASE="$scratch/dns/healthy" DNS_CASE=healthy \
        E2FSPROGS_VERSION="$version" TMPDIR="$scratch/dns/healthy" \
        bash -c 'cd() { builtin cd "$(dirname "$CASE")/repo"; }; source /dev/stdin' \
        <<<"guest_setup=fixtures
$cold_setup" 2>&1) || status=$?
    [[ "$output" == *'apt-get install -y e2fsprogs attr acl fdisk util-linux mount kmod'*'modprobe loop'* ]] ||
        note "fixtures $version: missing fixture packages or loop setup"
    [[ "$output" != *vm-setup* && "$output" != *toolchain-setup* ]] ||
        note "fixtures $version: full preparation ran"
    if [ "$version" = 1.46.6 ]; then
        [ "$status" = 1 ] && [[ "$output" == *'requires e2fsprogs >= 1.47.0'* ]] ||
            note "fixtures $version: old e2fsprogs was accepted"
    else
        [ "$status" = 0 ] || note "fixtures $version: status $status, expected 0"
    fi
done

if [ "$fails" -ne 0 ]; then exit 1; fi
echo 'PASS  a detached guest setup outlives SSH stalls and fails only for a stated reason'
