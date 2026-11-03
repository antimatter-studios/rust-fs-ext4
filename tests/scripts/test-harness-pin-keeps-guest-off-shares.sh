#!/usr/bin/env bash
# The harness this repository pins keeps a Mac-hosted guest off the shares.
#
# fs-linux-test-harness before v0.5.3 runs the notify-forwarder v1.1.0
# receiver in a guest on a Mac host. It answers a host file event by opening
# the named file read-write: it writes the first byte back, and it writes one
# byte to a file it reads as empty and truncates it to 0 (harness #84, #87).
# When a test's write landed between that read and that truncate, the file
# ended empty though every call in the test returned success. In
# `chore test:vm` that was a mount of a fresh fixture copy failing with
# "failed to fill whole buffer": 57 of 600 runs of one target (#611).
#
# CI has no Mac host, so no CI job can see the emptied file. This guard holds
# the pin that decides it.
#
#   bash tests/scripts/test-harness-pin-keeps-guest-off-shares.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CHORES="$REPO/chores.yml"
FLOOR="v0.5.3"

fails=0
fail() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

# Whether version $1 is at or above version $2, field by field.
at_or_above() {
    local a b
    IFS=. read -r -a a <<<"${1#v}"
    IFS=. read -r -a b <<<"${2#v}"
    for i in 0 1 2; do
        [ "${a[$i]:-0}" -gt "${b[$i]:-0}" ] && return 0
        [ "${a[$i]:-0}" -lt "${b[$i]:-0}" ] && return 1
    done
    return 0
}

# The comparison has to be able to fail, and to order 0.10 above 0.5.
at_or_above v0.5.3 "$FLOOR" || fail "v0.5.3 reads as below the floor $FLOOR"
at_or_above v0.10.0 "$FLOOR" || fail "v0.10.0 reads as below the floor $FLOOR"
at_or_above v1.0.0 "$FLOOR" || fail "v1.0.0 reads as below the floor $FLOOR"
if at_or_above v0.5.2 "$FLOOR"; then fail "v0.5.2 reads as at or above the floor $FLOOR"; fi
if at_or_above v0.4.0 "$FLOOR"; then fail "v0.4.0 reads as at or above the floor $FLOOR"; fi

pin="$(sed -nE 's/^ *LINUX_HARNESS_REF: *(v[0-9]+\.[0-9]+\.[0-9]+) *$/\1/p' "$CHORES")"
if [ -z "$pin" ]; then
    fail "chores.yml has no LINUX_HARNESS_REF of the form vX.Y.Z"
elif ! at_or_above "$pin" "$FLOOR"; then
    fail "LINUX_HARNESS_REF is $pin; below $FLOOR a Mac-hosted guest writes to the shares and can empty a test's file (#611)"
fi

if [ "$fails" -gt 0 ]; then
    exit 1
fi
echo "PASS  the harness pin $pin keeps a Mac-hosted guest off the shares"
