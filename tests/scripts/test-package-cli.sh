#!/usr/bin/env bash
# The release tarball has the layout an installer copies as-is --
# bin/mkfs.ext4, share/rust-fs-ext4/CAVEATS and the licences, nothing else --
# and the tool in it runs and identifies itself.
#
# Cargo refuses a dot in a target name, so the formatter builds as
# `mkfs_ext4`. scripts/package-cli.sh renames it to `mkfs.ext4` before
# packaging; the underscore is a build-system constraint and must not reach
# a public artifact.
#
# This runs the real packaging script against stand-in binaries in a
# sandbox: one that behaves, and one for each way a build can be wrong
# (missing, --help failing, reporting a version other than the tag's). The
# release workflow runs the same script against the real binary on every
# platform it publishes, so the checks here are the checks a release makes.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PACKAGE="$ROOT/scripts/package-cli.sh"
pass=0
fail=0

ok()  { pass=$((pass + 1)); }
bad() { fail=$((fail + 1)); printf 'FAIL %s\n' "$*"; }

sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT

crate="$(sed -n 's/^name = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -n 1)"
[ "$crate" = "am-fs-ext4" ] && ok || bad "crate name read from Cargo.toml: '$crate'"

# A stand-in for the built formatter. $1 is the version it reports, $2 the
# exit status of --help.
stub() {
    local path="$sandbox/$3"
    mkdir -p "$(dirname "$path")"
    cat > "$path" <<STUB
#!/usr/bin/env bash
case "\$1" in
    --help)    echo "Usage: mkfs.ext4 [options] <device>"; exit $2 ;;
    --version) echo "mkfs.ext4 ($crate) $1" ;;
    *)         exit 2 ;;
esac
STUB
    chmod +x "$path"
    printf '%s\n' "$path"
}

# Runs the packaging script in a fresh output directory; prints its stdout.
package() {
    local out="$sandbox/out-$RANDOM$RANDOM"
    mkdir -p "$out"
    (cd "$out" && bash "$PACKAGE" "$@" 2>"$sandbox/stderr")
}

[ -f "$PACKAGE" ] && ok || bad "scripts/package-cli.sh exists"

# --- A good build: the tarball, its name, and exactly its contents. -------
good="$(stub 9.9.9 0 good/mkfs_ext4)"
if tarball="$(package 9.9.9 darwin-arm64 "$(dirname "$good")")"; then
    ok
else
    bad "a good build packages: $(cat "$sandbox/stderr")"
    tarball=""
fi

case "$(basename "$tarball")" in
    "$crate-9.9.9-darwin-arm64.tar.gz") ok ;;
    *) bad "tarball is named <crate>-<version>-<label>.tar.gz, got '$tarball'" ;;
esac

if [ -f "$tarball" ]; then
    listing="$(tar -tzf "$tarball" | sort | tr '\n' ' ')"
    files="$(tar -tzf "$tarball" | sed 's|^\./||' | grep -v '/$' | sort | tr '\n' ' ')"
    [ "$files" = "LICENSE bin/mkfs.ext4 share/rust-fs-ext4/CAVEATS " ] && ok \
        || bad "tarball holds exactly bin/mkfs.ext4, the CAVEATS and the licence, got: $files"

    case "$listing" in
        *mkfs_ext4*) bad "the cargo target name reached the tarball: $listing" ;;
        *) ok ;;
    esac
    unpacked="$sandbox/unpacked"
    mkdir -p "$unpacked"
    tar -xzf "$tarball" -C "$unpacked"
    [ -x "$unpacked/bin/mkfs.ext4" ] && ok || bad "bin/mkfs.ext4 is executable in the tarball"
    cmp -s "$unpacked/share/rust-fs-ext4/CAVEATS" "$ROOT/packaging/CAVEATS" && ok \
        || bad "share/rust-fs-ext4/CAVEATS is packaging/CAVEATS"
    cmp -s "$unpacked/LICENSE" "$ROOT/LICENSE" && ok || bad "LICENSE is the repository's"
    cmp -s "$unpacked/bin/mkfs.ext4" "$good" && ok || bad "bin/mkfs.ext4 is the built binary, renamed"
fi

# --- Each way a build can be wrong is refused, with no tarball left. ------
refused() {
    local why="$1"; shift
    local out
    if out="$(package "$@")"; then
        bad "$why is refused, but packaging succeeded: $out"
    else
        ok
        [ -z "$out" ] && ok || bad "$why leaves no tarball named on stdout: $out"
    fi
}

refused "a missing binary" 9.9.9 darwin-arm64 "$sandbox/nowhere"
refused "a binary whose --help fails" 9.9.9 darwin-arm64 "$(dirname "$(stub 9.9.9 1 helpfails/mkfs_ext4)")"
refused "a binary reporting a version other than the tag's" 9.9.9 darwin-arm64 "$(dirname "$(stub 1.0.0 0 wrongver/mkfs_ext4)")"
refused "a missing label" 9.9.9 "" "$(dirname "$good")"
refused "a missing version" "" darwin-arm64 "$(dirname "$good")"

# --- The release workflow packages through this script. ------------------
release="$ROOT/.github/workflows/release.yml"
grep -q 'scripts/package-cli.sh' "$release" && ok \
    || bad "release.yml packages through scripts/package-cli.sh"
grep -q 'cargo build --release --locked --bin mkfs_ext4' "$release" && ok \
    || bad "release.yml builds the mkfs_ext4 target"
grep -qE 'uses: actions/attest-build-provenance@[0-9a-f]{40}' "$release" && ok \
    || bad "release.yml attests the tarballs' build provenance, with the action pinned to a commit"
grep -q 'attestations: write' "$release" && ok \
    || bad "release.yml grants the release job attestations: write"

printf 'package-cli: %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
