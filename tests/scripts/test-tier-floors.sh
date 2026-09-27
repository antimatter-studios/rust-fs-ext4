#!/usr/bin/env bash
# Every cargo tier carries a MEASURED floor, and a tier that collapses to
# almost nothing fails it (#271).
#
# The tiers select their targets by grep (scripts/test-targets.sh). A
# predicate edited or a helper renamed shrinks a selection, `cargo test`
# runs what is left and exits 0, and tier.sh reports `ok`. A floor of 1
# catches only the collapse to exactly zero; a tier whose selection shrank
# to one stray target sails through it. So this runs each floor that
# chores.yml declares against a log in which the tier executed ONE test --
# the shape of a selection that stopped matching all but one file -- and
# requires the floor to refuse it.
#
# And every tier that tier.sh runs must have a floor at all, so a tier
# added later cannot arrive without one. Two tiers are exceptions, named
# below, because neither is a cargo test run and the counter reads cargo's
# result lines: `scripts` prints PASS lines, so it would read zero for a
# run that did everything; and `semver` runs cargo-semver-checks, whose
# check count is legitimately ZERO once the version declares a break
# ("0 checks: 0 pass, 254 skip", then "no semver update required"), so no
# floor above zero could hold for it (#120).
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CHORES="$REPO/chores.yml"
UNFLOORED='scripts semver'

fails=0
fail() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT
mkdir -p "$sandbox/scripts" "$sandbox/tmp/logs"
cp "$REPO/scripts/test-floor.sh" "$sandbox/scripts/test-floor.sh"

# `scripts/test-floor.sh <tier> <floor>`, wherever chores.yml calls it.
floors="$(grep -oE 'scripts/test-floor\.sh +[a-z0-9_-]+ +[0-9]+' "$CHORES" |
    awk '{ print $2, $3 }')"
if [ -z "$floors" ]; then
    fail "chores.yml calls scripts/test-floor.sh nowhere"
fi

# `scripts/tier.sh <label> <tier> <lines> <bytes>`; the label may be quoted.
tiers="$(grep -oE 'scripts/tier\.sh +("[^"]*"|[^ ]+) +[a-z0-9_-]+ +[0-9]+ +[0-9]+' "$CHORES" |
    sed -E 's/^scripts\/tier\.sh +("[^"]*"|[^ ]+) +([a-z0-9_-]+) .*/\2/' | sort -u)"
if [ -z "$tiers" ]; then
    fail "chores.yml runs no tier through scripts/tier.sh"
fi

for tier in $tiers; do
    case " $UNFLOORED " in *" $tier "*) continue ;; esac
    if ! printf '%s\n' "$floors" | grep -qE "^$tier "; then
        fail "the $tier tier has no scripts/test-floor.sh call in chores.yml"
    fi
done

while read -r tier floor; do
    [ -n "$tier" ] || continue
    log="$sandbox/tmp/logs/$tier.log"
    printf '%s\n' \
        "running 1 test" \
        "test stray ... ok" \
        "" \
        "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s" \
        >"$log"
    if bash "$sandbox/scripts/test-floor.sh" "$tier" "$floor" >/dev/null 2>&1; then
        fail "the $tier tier went green having executed 1 test (floor $floor)"
    fi
done <<<"$floors"

if [ "$fails" -gt 0 ]; then
    exit 1
fi
echo "PASS  every cargo tier has a floor, and each refuses a tier that ran 1 test"
