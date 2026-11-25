#!/usr/bin/env bash
# The host runner can read a served guest's evidence even when the runner
# container dies before its EXIT trap hands the files back.
#
# `scripts/ci-qemu-runner.sh serve` runs scripts/ci-qemu-benchmark.sh as root
# in a container, over the workspace the host runner owns. The benchmark makes
# its evidence directory with `mktemp -d`: root's, mode 0700. Ownership came
# back only from the container's EXIT trap. In run 45194071114 (job
# 137789233204, runner hsvj6, head 51621312) Docker returned 125, "error
# waiting for container: unexpected EOF", before the guest was SSH-ready; the
# trap never ran, and actions/upload-artifact then stopped with "EACCES:
# permission denied, scandir" on tmp/qemu-benchmark/x86_64-tcg.TrYokc, so no
# evidence was kept.
#
# This runs the benchmark's own lines that make the evidence directory as root
# in a container, writes evidence and a private key into it the way the
# benchmark does, and kills the shell with SIGKILL so no trap runs. Then, as
# the runner's uid:
#   - the directory can be listed and the evidence files read;
#   - the private key still cannot be read;
#   - the directory is still mode 0700, so another uid can list nothing.
#
#   bash scripts/test-ci-qemu-evidence-owner.sh
#
# Not in tests/scripts/: it needs Docker, and prints the probe only on a failure.
#
# Needs Docker and an image with bash and setpriv (TEST_EVIDENCE_IMAGE,
# default ubuntu:24.04). Without them it fails; it never skips.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${TEST_EVIDENCE_IMAGE:-ubuntu:24.04}"

fail() { echo "FAIL  $*" >&2; exit 1; }

command -v docker >/dev/null 2>&1 || fail "docker is required to run a root container"
docker info >/dev/null 2>&1 || fail "the Docker daemon is not reachable"

# The benchmark's lines from its first tmp/qemu-benchmark mkdir up to the
# first use of the evidence directory: the lines that make it.
snippet="$(awk '
    /^mkdir -p tmp\/qemu-benchmark tmp\/qemu-images$/ { on = 1 }
    /^disk="\$state\/guest.qcow2"$/ { exit }
    on { print }
' "$REPO/scripts/ci-qemu-benchmark.sh")"
case "$snippet" in
    *'mktemp -d'*) ;;
    *) fail "could not find where scripts/ci-qemu-benchmark.sh makes its evidence directory" ;;
esac

output="$(docker run --rm -e SNIPPET="$snippet" "$IMAGE" bash -c '
    set -u
    runner=1001:1001
    # The host runner made the workspace and tmp/qemu-benchmark before Docker ran.
    mkdir -p /w/tmp/qemu-benchmark && chown -R "$runner" /w
    EVIDENCE_OWNER="$runner" bash -c '\''
        set -euo pipefail
        repo=/w; action=serve; arch=x86_64; accel=tcg
        cd "$repo"
        # The ownership trap the abrupt exit skips.
        trap "chown -R \"\$EVIDENCE_OWNER\" /w/tmp" EXIT
        eval "$SNIPPET"
        printf "%s\n" "$state" >/run/state-path
        printf "booting guest\n" >"$state/phase"
        printf "boot_budget_seconds=1800\n" >"$state/boot-policy.txt"
        printf "{}\n" >"$state/cache-keys.json"
        printf "console\n" >"$state/console.log"
        mkdir "$state/seed"
        printf "seed\n" >"$state/seed/seed.log"
        (umask 077; printf "PRIVATE KEY\n" >"$state/key")
        kill -KILL $$
    '\''
    printf "root-shell-status=%s\n" "$?"
    state="$(cat /run/state-path)" || { echo "no-state-path"; exit 0; }
    printf "mode=%s owner=%s\n" "$(stat -c %a "$state")" "$(stat -c %u:%g "$state")"
    as_runner() { setpriv --reuid=1001 --regid=1001 --clear-groups "$@"; }
    if found="$(as_runner find "$state" -type f \( -name "*.json" -o -name "*.txt" -o -name "*.log" \) 2>&1)"; then
        printf "runner-scan=ok files=%s\n" "$(printf "%s\n" "$found" | wc -l)"
    else
        printf "runner-scan=denied %s\n" "$found"
    fi
    if as_runner cat "$state/console.log" "$state/seed/seed.log" >/dev/null 2>&1; then
        echo "runner-read=ok"
    else
        echo "runner-read=denied"
    fi
    if as_runner cat "$state/key" >/dev/null 2>&1; then echo "runner-key=readable"; else echo "runner-key=denied"; fi
    if setpriv --reuid=1002 --regid=1002 --clear-groups ls "$state" >/dev/null 2>&1; then
        echo "other-scan=ok"
    else
        echo "other-scan=denied"
    fi
' 2>&1)" || fail "the container did not run: $output"
expect() {
    printf '%s\n' "$output" | grep -qx -- "$1" || fail "$2 (wanted '$1'); the probe printed:
$output"
}
expect 'root-shell-status=137' "the root shell was not killed by SIGKILL, so the trap may have run"
expect 'mode=700 owner=1001:1001' "the evidence directory is not the runner's, mode 0700, at creation"
expect 'runner-scan=ok files=4' "the runner cannot scan the evidence directory after an abrupt exit"
expect 'runner-read=ok' "the runner cannot read the evidence files"
expect 'runner-key=denied' "the private key became readable to the runner"
expect 'other-scan=denied' "another uid can list the evidence directory"
echo "PASS  evidence stays readable to the runner, and private to it, after an abrupt exit"
