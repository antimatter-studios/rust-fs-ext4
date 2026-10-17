#!/usr/bin/env bash
# No tests/cli file checks an exit status that a command substitution has
# already replaced.
#
#   check "write exits 0 ($(cat "$err"))" test $? -eq 0
#
# reads like a check of the tool's exit status and is not one: the words of
# a command are expanded left to right, so the `cat` in the description
# runs first, and `$?` is then cat's status -- 0, whatever the tool did.
# The check passes for a tool that failed, which is the one thing it was
# written to catch. Capture the status on the line before (`rc=$?`), then
# test that.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# A line that expands a command substitution and then reads `$?`.
PATTERN='\$\(.*\$\?'

fail() { echo "FAIL  $*" >&2; exit 1; }

# The scan has to see the shape it refuses, and not a line that captures
# the status first.
mkdir -p "$REPO/tmp"
sandbox="$(mktemp -d "$REPO/tmp/cli-status.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT HUP INT TERM
printf '%s\n' 'check "exits 0 ($(cat "$e"))" test $? -eq 0' >"$sandbox/bad.sh"
printf '%s\n' 'rc=$?' 'check "exits 0 ($(cat "$e"))" test "$rc" -eq 0' >"$sandbox/good.sh"
grep -qE "$PATTERN" "$sandbox/bad.sh" || fail "the scan does not see a status read after a substitution"
! grep -qE "$PATTERN" "$sandbox/good.sh" || fail "the scan refuses a status captured first"

# Exercise the check helper as well as the scan. These are isolated probes:
# no installed filesystem command is run or replaced in a CLI test file.
# The old shape accepts false; capturing first must reject false and accept
# true, so neither a broken assertion nor one that always fails can pass.
probe() {
    local command="$1" assertion="$2"
    bash -c '
        source "$1/tests/cli/lib.sh"
        e="$SANDBOX/err"
        : >"$e"
        "$2"
        eval "$3"
        finish
    ' "$REPO/tests/cli/status-probe.sh" "$REPO" "$command" "$assertion" \
        >"$sandbox/probe.out" 2>&1
}

bad="$(cat "$sandbox/bad.sh")"
good="$(cat "$sandbox/good.sh")"
probe false "$bad" || fail "the old shape no longer reproduces the false positive"
if probe false "$good"; then
    fail "a captured status accepted false: $(cat "$sandbox/probe.out")"
fi
grep -q 'test result: FAILED. 0 passed; 1 failed' "$sandbox/probe.out" ||
    fail "the probe failed without rejecting the status: $(cat "$sandbox/probe.out")"
probe true "$good" || fail "a captured status rejected true: $(cat "$sandbox/probe.out")"

found="$(grep -nE "$PATTERN" "$REPO"/tests/cli/*.sh || true)"
if [ -n "$found" ]; then
    echo "FAIL  these read \$? after a command substitution has replaced it; capture it first (rc=\$?):" >&2
    printf '%s\n' "$found" >&2
    exit 1
fi
echo "PASS  every exit status a tests/cli file checks is the tool's"
