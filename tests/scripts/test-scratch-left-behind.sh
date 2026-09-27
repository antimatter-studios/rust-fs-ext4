#!/usr/bin/env bash
# A test run leaves nothing behind in tmp/ (#331).
#
# tmp/ is where every image a test writes lives, and nothing else ever
# empties it, so a test that keeps its image grows it by megabytes a run.
# scripts/test.sh removes the scratch directory it made; a test that keeps
# its image on purpose does so only when RFE_KEEP_IMAGES asks it to.
#
# Driven through a real test binary, the one that used to keep its image
# unconditionally: run once into a scratch directory this script owns
# (test.sh does not delete a caller's directory, so what the test left is
# visible), once the default way (no tmp/fs-ext4-tests.* may be left), and
# once with RFE_KEEP_IMAGES set, which must keep the image -- the escape
# hatch is part of the contract, and a guard that only ever sees an empty
# directory cannot tell "cleaned up" from "never wrote".
#
# Needs the fixtures (test-disks/*.img); without them the test binary fails
# and names `chore fixtures`, and so does this.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TEST=repro_wants_dir_symlinks

fails=0
note() { echo "FAIL  $*" >&2; fails=$(( fails + 1 )); }

mkdir -p "$REPO/tmp"
work="$(mktemp -d "$REPO/tmp/scratch-left-behind.XXXXXX")"
trap 'rm -rf "$work"' EXIT
log="$work/cargo.log"

# run DIR [ENV...]: the test binary through scripts/test.sh, quietly.
run() {
    local dir="$1"
    shift
    if ! env -u RFE_KEEP_IMAGES -u FS_EXT4_TEST_TMPDIR "$@" ${dir:+FS_EXT4_TEST_TMPDIR="$dir"} \
        "$REPO/scripts/test.sh" --locked --release --test "$TEST" > "$log" 2>&1; then
        note "cargo test --test $TEST failed; the tail of its log:"
        tail -n 15 "$log" >&2
        return 1
    fi
}

listing() { find "$1" -mindepth 1 | LC_ALL=C sort; }
runs() { find "$REPO/tmp" -maxdepth 1 -name 'fs-ext4-tests.*' | LC_ALL=C sort; }

# --- 1. The test deletes what it wrote. --------------------------------------
mkdir -p "$work/exact"
if run "$work/exact"; then
    left="$(listing "$work/exact")"
    [ -z "$left" ] || note "$TEST left these in its scratch directory: $(echo $left)"
fi

# --- 2. The default run leaves no tmp/fs-ext4-tests.* behind. -----------------
before="$(runs)"
if run ""; then
    new="$(comm -13 <(printf '%s\n' "$before") <(printf '%s\n' "$(runs)") | sed '/^$/d')"
    [ -z "$new" ] || note "a default scripts/test.sh run left $(echo $new)"
fi

# --- 3. RFE_KEEP_IMAGES keeps them. -------------------------------------------
mkdir -p "$work/keep"
if run "$work/keep" RFE_KEEP_IMAGES=1; then
    count="$(listing "$work/keep" | grep -c '\.img$')"
    [ "$count" -eq 2 ] || note "RFE_KEEP_IMAGES=1 kept $count images, not the test's 2"
fi

if [ "$fails" -gt 0 ]; then
    exit 1
fi
echo "PASS  a test run leaves nothing in tmp/ unless RFE_KEEP_IMAGES asks it to"
