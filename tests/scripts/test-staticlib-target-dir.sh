#!/usr/bin/env bash
# `chore staticlib` copies the archive from where cargo built it (#624).
#
# cargo decides where its output goes: ./target by default, or wherever
# CARGO_TARGET_DIR or `build.target-dir` in a config file says, and a relative
# value counts from the directory cargo runs in. The task has to copy from
# that place, so each case below runs the REAL task from chores.yml with real
# cargo, then compares dist/ with the archive cargo left where it was told to.
#
# A MISSING ARCHIVE IS THE LOUD FAILURE AND A STALE ONE IS THE QUIET ONE. A task
# that always reads ./target fails when ./target is empty, and with an old
# build in ./target it succeeds and ships that old archive. So one case plants
# a decoy there, and every case compares bytes rather than asking whether a
# file arrived.
#
# THE CRATE IS A STAND-IN of one function under the real package name, in a
# sandbox with a stand-in ../rust-fs-core beside it. What is under test is where
# the archive lands, which does not depend on what is in it, so this builds in
# seconds and needs no sibling: it can run on every pull request. Whether the
# real crate's archive is fresh is test-staticlib-freshness.sh's question.
#
# TRIPLE is the host's, as in test-staticlib-freshness.sh: a Linux runner
# cannot link for the Apple triple the task names by default.
#
#   bash tests/scripts/test-staticlib-target-dir.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LIB=fs_ext4

fails=0
fail() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

# NOTHING SKIPS: what this needs and cannot find is a failure naming the
# task that provides it.
command -v chore >/dev/null 2>&1 || { echo "FAIL  chore is not on PATH (scripts/ci-install-chore.sh)" >&2; exit 1; }
command -v cargo >/dev/null 2>&1 || { echo "FAIL  cargo is not on PATH (rust-toolchain.toml names the toolchain)" >&2; exit 1; }
triple="$(rustc -vV | sed -n 's/^host: //p')"
[ -n "$triple" ] || { echo "FAIL  rustc -vV names no host triple" >&2; exit 1; }

sandbox="$REPO/tmp/staticlib-target-dir"
log="$REPO/tmp/logs/staticlib-target-dir.log"
[ ! -e "$sandbox" ] || rm -r "$sandbox"
mkdir -p "$sandbox" "$(dirname "$log")"
trap '[ ! -e "$sandbox" ] || rm -r "$sandbox"' EXIT
: > "$log"

# The caller's own setting must not decide a case, and neither may a target
# directory in the caller's ~/.cargo/config.toml: each case states its own.
unset CARGO_TARGET_DIR CARGO_BUILD_TARGET_DIR
export CARGO_HOME="$sandbox/cargo-home"

# A fresh crate per case, so no case sees another's dist/ or fingerprint.
crate=""
fresh_crate() {
    crate="$sandbox/$1/rust-fs-ext4"
    mkdir -p "$crate/src" "$crate/include" "$sandbox/$1/rust-fs-core/include"
    cp "$REPO/chores.yml" "$REPO/rust-toolchain.toml" "$crate/"
    cp "$REPO/include/$LIB.h" "$crate/include/"
    : > "$sandbox/$1/rust-fs-core/include/fs_core.h"
    # [workspace] stops cargo walking up into this repository's own manifest.
    cat > "$crate/Cargo.toml" <<EOF
[package]
name = "fs-ext4"
version = "0.0.0"
edition = "2021"

[lib]
name = "$LIB"
crate-type = ["staticlib"]

[workspace]
EOF
    printf '#[no_mangle]\npub extern "C" fn fs_ext4_target_dir_probe() -> u32 { 624 }\n' > "$crate/src/lib.rs"
    (cd "$crate" && cargo generate-lockfile --offline) >> "$log" 2>&1 ||
        fail "$1: cargo generate-lockfile failed; see $log"
}

# build_and_compare LABEL EXPECTED-TARGET-DIR [VAR=VALUE...]
build_and_compare() {
    local label="$1" target_dir="$2"
    shift 2
    local built="$target_dir/$triple/release/lib$LIB.a" shipped="$crate/dist/lib$LIB.a" output
    echo "== $label" >> "$log"
    if ! output="$(cd "$sandbox" && env ${1+"$@"} chore -C "$crate" staticlib "TRIPLE=$triple" 2>&1)"; then
        printf '%s\n' "$output" >> "$log"
        fail "$label: chore staticlib failed; see $log"
        printf '%s\n' "$output" | tail -n 4 | sed 's/^/      /' >&2
        return
    fi
    printf '%s\n' "$output" >> "$log"
    [ -f "$built" ] || { fail "$label: cargo left no archive at $built"; return; }
    [ -f "$shipped" ] || { fail "$label: chore staticlib left no dist/lib$LIB.a"; return; }
    cmp -s "$built" "$shipped" || fail "$label: dist/lib$LIB.a is not the archive cargo built at $built"
}

# 1. Nothing set: cargo builds in ./target.
fresh_crate unset
build_and_compare "CARGO_TARGET_DIR unset" "$crate/target"

# 2. An absolute directory outside the crate, nothing in ./target.
fresh_crate absolute
build_and_compare "absolute CARGO_TARGET_DIR" "$sandbox/absolute-target" "CARGO_TARGET_DIR=$sandbox/absolute-target"

# 3. A relative directory, which counts from the crate, where cargo runs: the
#    command starts from the sandbox, so it is not the caller's directory.
fresh_crate relative
build_and_compare "relative CARGO_TARGET_DIR" "$crate/relative-target" "CARGO_TARGET_DIR=relative-target"

# 4. A relative directory that leaves the crate.
fresh_crate parent
build_and_compare "relative CARGO_TARGET_DIR above the crate" "$sandbox/parent/parent-target" "CARGO_TARGET_DIR=../parent-target"

# 5. An old archive in ./target while cargo builds elsewhere: it must not ship.
fresh_crate decoy
mkdir -p "$crate/target/$triple/release"
echo "an old build, not the archive cargo was asked for" > "$crate/target/$triple/release/lib$LIB.a"
build_and_compare "an old archive in ./target" "$sandbox/decoy-target" "CARGO_TARGET_DIR=$sandbox/decoy-target"

# 6. Not an environment variable at all: cargo's own configuration.
fresh_crate config
mkdir -p "$crate/.cargo"
printf '[build]\ntarget-dir = "config-target"\n' > "$crate/.cargo/config.toml"
build_and_compare "build.target-dir in .cargo/config.toml" "$crate/config-target"

if [ "$fails" -gt 0 ]; then
    echo "chore output: $log" >&2
    exit 1
fi
echo "PASS  chore staticlib copies the archive from where cargo built it"
