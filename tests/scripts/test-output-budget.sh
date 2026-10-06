#!/usr/bin/env bash
# The output rule, checked: EVERY TEST TIER IS BUDGETED, and the budget can
# actually fail a run.
#
# Quiet-by-default is a convention, and a convention rots in a week. This is
# the number that fails the build instead: a tier added later without going
# through ../rust-fs-core/scripts/tier.sh, or given a budget of zero (which output-budget.sh
# reads as "no budget"), fails here rather than being noticed the next time
# somebody scrolls past three thousand lines.
#
# See the fs-linux-test-harness README, "Output: quiet by default, --verbose
# on request", and the measured budget table in chores.yml.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# tmp/ is gitignored, so a fresh checkout does not have one and every mktemp
# below fails into the filesystem root -- which reads as nine unrelated
# assertion failures rather than "there is no tmp/".
mkdir -p "$REPO/tmp"
fails=0
note() { echo "FAIL  $*" >&2; fails=$(( fails + 1 )); }

# --- 1. Every tier runs its tests through tier.sh, with a real budget. ------
#
# The tiers, by task name, and the log each one is required to write. This
# list is the contract; a tier not named here is not checked, so adding one
# means adding it here too -- which is the one registration this file asks
# for, and it is in the same file as the assertion.
for tier in test:unit test:images test:oracle test:kernel test:lwext4 test:wasm test:vm; do
    # The task's own block: from its name to the next top-level task.
    block="$(awk -v t="  $tier:" '
        $0 == t { inside = 1; next }
        inside && /^  [a-z][a-z:_-]*:$/ { inside = 0 }
        inside { print }
    ' "$REPO/chores.yml")"

    if [ -z "$block" ]; then
        note "chores.yml has no task '$tier' (or it moved) -- the budget check cannot see it"
        continue
    fi
    case "$block" in
        *../rust-fs-core/scripts/tier.sh*) ;;
        *) note "$tier does not run through ../rust-fs-core/scripts/tier.sh, so its output is unbounded" ;;
    esac
    # tier.sh LABEL LOG MAX-LINES MAX-BYTES: a zero in either position is
    # "no budget" to output-budget.sh, which is the shape this test exists
    # to refuse.
    if ! printf '%s\n' "$block" | grep -Eq "scripts/tier\.sh +[^ ]+ +[^ ]+ +[1-9][0-9]* +[1-9][0-9]*"; then
        note "$tier calls tier.sh without two non-zero budgets (lines and bytes)"
    fi
done

# --- 2. A budget that is breached fails the run. ---------------------------
# The wrapper is rust-fs-core's, run in place from the sibling the way its
# tier.sh runs it.
budget="$REPO/../rust-fs-core/scripts/output-budget.sh"
if [ ! -f "$budget" ]; then
    note "../rust-fs-core/scripts/output-budget.sh is missing -- run 'chore siblings'"
else
    work="$(mktemp -d "$REPO/tmp/output-budget-test.XXXXXX")"
    trap 'rm -rf "$work"' EXIT

    # Passed, but printed too much: status 65, distinct from a failing suite.
    out="$("$budget" --log "$work/loud.log" --max-lines 5 --label loud \
              -- sh -c 'i=0; while [ $i -lt 40 ]; do echo line $i; i=$((i+1)); done' 2>&1)"
    rc=$?
    [ "$rc" = 65 ] || note "a run over its line budget exited $rc, not 65"
    case "$out" in *"printed 40 lines (budget 5)"*) ;; *) note "the breach did not name the count: $out" ;; esac
    [ "$(wc -l < "$work/loud.log" | tr -d ' ')" = 40 ] || note "the log did not keep every line"

    # Under budget: one verdict line, and the output is NOT on the terminal.
    out="$("$budget" --log "$work/quiet.log" --max-lines 5 --label quiet -- echo hello 2>&1)"
    rc=$?
    [ "$rc" = 0 ] || note "a run inside its budget exited $rc"
    case "$out" in *hello*) note "a passing run put its output on the terminal: $out" ;; esac
    case "$out" in *"quiet: ok"*) ;; *) note "a passing run printed no verdict: $out" ;; esac

    # Failed: the excerpt, and the command's own status.
    out="$("$budget" --log "$work/bad.log" --max-lines 5 --tail 3 --label bad \
              -- sh -c 'echo the reason; exit 7' 2>&1)"
    rc=$?
    [ "$rc" = 7 ] || note "a failing run exited $rc, not the command's own 7"
    case "$out" in *"the reason"*) ;; *) note "a failure printed no excerpt: $out" ;; esac

    # A failure is quiet unless the tail is asked for. Core v0.2.13 stopped
    # reading the log aloud on every failure; this is the shape that replaced
    # it, and it is the one every CI log now carries.
    out="$("$budget" --log "$work/silent.log" --label silent \
              -- sh -c 'echo the reason; exit 7' 2>&1)"
    rc=$?
    [ "$rc" = 7 ] || note "a quiet failure exited $rc, not the command's own 7"
    case "$out" in *"the reason"*) note "a failure read its log aloud without --tail: $out" ;; esac
    case "$out" in *"silent.log"*) ;; *) note "a quiet failure did not name its log: $out" ;; esac

    # Verbose streams, and is still budgeted.
    out="$(OUTPUT_BUDGET_VERBOSE=1 "$budget" --log "$work/v.log" --max-lines 5 --label v -- echo hello 2>&1)"
    case "$out" in *hello*) ;; *) note "--verbose did not stream the output: $out" ;; esac
fi

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails output-budget violation(s)" >&2
    exit 1
fi
echo "PASS  every test tier is budgeted, and a breached budget fails the run"
