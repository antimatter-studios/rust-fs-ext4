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

found="$(grep -nE "$PATTERN" "$REPO"/tests/cli/*.sh || true)"
if [ -n "$found" ]; then
    echo "FAIL  these read \$? after a command substitution has replaced it; capture it first (rc=\$?):" >&2
    printf '%s\n' "$found" >&2
    exit 1
fi
echo "PASS  every exit status a tests/cli file checks is the tool's"
