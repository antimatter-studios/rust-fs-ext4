#!/usr/bin/env bash
# `chore staticlib` never calls a stale or broken artifact up to date (#330).
#
# The task skips its work when its `sources:` fingerprint is unchanged and
# every `generates:` file exists. So each input the build reads must be a
# source, each file it ships must be generated, and the build must not
# re-resolve dependencies behind the lock. Three cases, each run against the
# REAL task definition in chores.yml:
#
#   1. fs_core.h deleted from dist/  -> the task runs again and restores it
#      (fs_ext4.h #includes it, so a dist/ without it does not compile).
#   2. a rust-fs-core source edited -> the task rebuilds, and the new code
#      is in the archive (not an old .a linking old core code).
#   3. the core requirement bumped without updating Cargo.lock -> the build
#      FAILS and leaves the lock untouched (--locked), rather than shipping
#      dependency versions CI never tested.
#
# IT RUNS IN A SANDBOX, a copy of this crate and of ../rust-fs-core side by
# side, the copy of core patched in for the published one, so case 2 and
# case 3 can edit core and the manifest without touching a real checkout. The sandbox has no ../fs-linux-test-harness beside it, so
# chore's after_all reaper (chores.yml `lifecycle:`) does nothing: this test
# cannot stop a VM some other run is using.
#
# TRIPLE is the host's, passed on the command line: the build is the task's
# own, but a Linux runner cannot link the crate's binary for the Apple
# triple the file names. What is under test is the freshness contract, which
# does not depend on the triple.
#
#   bash tests/scripts/test-staticlib-freshness.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CORE="$REPO/../rust-fs-core"
LIB=fs_ext4

fails=0
fail() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

# NOTHING SKIPS: what this needs and cannot find is a failure naming the
# task that provides it.
command -v chore >/dev/null 2>&1 || { echo "FAIL  chore is not on PATH (scripts/ci-install-chore.sh)" >&2; exit 1; }
[ -f "$CORE/Cargo.toml" ] || { echo "FAIL  ../rust-fs-core is missing (chore siblings)" >&2; exit 1; }
triple="$(rustc -vV | sed -n 's/^host: //p')"
[ -n "$triple" ] || { echo "FAIL  rustc -vV names no host triple" >&2; exit 1; }

# Inside the repository's tmp/ (gitignored), at a stable path; the cargo
# output is kept under target/ so a rerun recompiles only the two crates.
sandbox="$REPO/tmp/staticlib-freshness"
cache="$REPO/target/staticlib-freshness"
log="$REPO/tmp/logs/staticlib-freshness.log"
rm -rf "$sandbox"
mkdir -p "$sandbox/rust-fs-ext4/tests" "$sandbox/rust-fs-core" "$cache" "$(dirname "$log")"
trap 'rm -rf "$sandbox"' EXIT
: > "$log"

ext4="$sandbox/rust-fs-ext4"
cp -R "$REPO/Cargo.toml" "$REPO/Cargo.lock" "$REPO/chores.yml" "$REPO/rust-toolchain.toml" \
    "$REPO/src" "$REPO/include" "$ext4/"
cp -R "$REPO/tests/support" "$ext4/tests/"
cp -R "$CORE/Cargo.toml" "$CORE/src" "$CORE/include" "$sandbox/rust-fs-core/"
[ -f "$CORE/rust-toolchain.toml" ] && cp "$CORE/rust-toolchain.toml" "$sandbox/rust-fs-core/"
ln -s "$cache" "$ext4/target"
# Cargo.toml names core by its published version; the sandbox builds the
# copied core instead, through the local override
# docs/developing-against-core.md describes, so case 2 can edit it. The lock
# is resolved once against the override, before anything is judged.
mkdir -p "$ext4/.cargo"
printf '[patch.crates-io]\nrust-fs-core = { path = "../rust-fs-core" }\n' > "$ext4/.cargo/config.toml"
(cd "$ext4" && cargo metadata --format-version 1 >/dev/null 2>>"$log") ||
    { echo "FAIL  the sandbox lock could not be resolved against the local core; see $log" >&2; exit 1; }

# Quiet: chore and cargo go to the log, which a failure names.
staticlib() {
    echo "== chore staticlib ($1)" >> "$log"
    chore -C "$ext4" staticlib "TRIPLE=$triple" >> "$log" 2>&1
}

if ! staticlib "first build"; then
    echo "FAIL  the first build failed; see $log" >&2
    exit 1
fi
for f in "lib$LIB.a" "include/$LIB.h" include/fs_core.h; do
    [ -f "$ext4/dist/$f" ] || fail "the first build did not produce dist/$f"
done

# 1. A shipped header deleted: the artifact is broken, so it is not up to date.
rm -f "$ext4/dist/include/fs_core.h"
staticlib "fs_core.h deleted" || fail "chore staticlib failed after dist/include/fs_core.h was deleted; see $log"
[ -f "$ext4/dist/include/fs_core.h" ] ||
    fail "dist/include/fs_core.h was deleted and chore staticlib called dist/ up to date without restoring it"

# 2. Core code changed: the archive must be rebuilt and carry the new code.
#    An exported symbol is the evidence, since cargo would rebuild an .a with
#    identical bytes for a comment-only edit.
probe="fs_core_freshness_probe_$$"
printf '\n#[no_mangle]\npub extern "C" fn %s() -> u32 { 330 }\n' "$probe" >> "$sandbox/rust-fs-core/src/lib.rs"
staticlib "core source edited" || fail "chore staticlib failed after a rust-fs-core source changed; see $log"
grep -qa "$probe" "$ext4/dist/lib$LIB.a" ||
    fail "a rust-fs-core source changed and dist/lib$LIB.a does not carry it: chore staticlib called a stale archive up to date"

# 3. The core requirement bumped, the lock not: the build must refuse.
cp "$ext4/Cargo.lock" "$sandbox/Cargo.lock.before"
sed -i.bak -E 's/^version = "[^"]+"/version = "0.2.999"/' "$sandbox/rust-fs-core/Cargo.toml"
sed -i.bak -E 's/(rust-fs-core = \{ version = )"[^"]+"/\1"=0.2.999"/' "$ext4/Cargo.toml"
grep -q 'version = "=0.2.999"' "$ext4/Cargo.toml" || fail "could not bump the rust-fs-core requirement in the sandbox Cargo.toml"
if staticlib "core requirement bumped, lock stale"; then
    fail "the core requirement moved without Cargo.lock and chore staticlib still built: the build is not --locked"
fi
cmp -s "$sandbox/Cargo.lock.before" "$ext4/Cargo.lock" ||
    fail "chore staticlib rewrote Cargo.lock instead of refusing a stale one"

if [ "$fails" -gt 0 ]; then
    echo "chore output: $log" >&2
    exit 1
fi
echo "PASS  chore staticlib rebuilds a broken dist/, rebuilds on a core change, and refuses a stale lock"
