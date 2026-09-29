#!/usr/bin/env bash
# package-cli.sh <version> <label> [target-dir]
#
# Package the built command-line tools as a release tarball in the current
# directory, check it, and print its file name on stdout.
#
#   <version>     the release version, without the leading `v`
#   <label>       the platform, e.g. darwin-arm64 or linux-x86_64
#   [target-dir]  where cargo put the release build (default: target/release)
#
# THE TARBALL IS THE CONTRACT with whatever installs it. Its layout is a
# prefix, so an installer copies it as-is and needs to know nothing about
# which tools are in it:
#
#   bin/<tool>...               every command-line tool, under its public name
#   share/<repo>/CAVEATS        a few lines an installer shows after install
#   LICENSE
#
# Man pages and shell completions, when there are any, go under share/ the
# same way. <repo> is the repository's name, from Cargo.toml's `repository`.
#
# TOOLS ARE RENAMED HERE. Cargo refuses a dot in a target name, so the
# formatter's target is `mkfs_ext4` and its public name is `mkfs.ext4`, the
# one the tooling convention uses and `mkfs -t ext4` resolves to. The
# underscore is a build-system constraint and has no business in a public
# artifact.
#
# THEN IT CHECKS WHAT IT BUILT, because a tarball whose tools do not run is
# worse than no tarball: the failure would surface as a user's bug report
# rather than a red release. The member list must be exactly the intended
# one, and every tool must answer --help and report `<tool> (<crate>)
# <version>` from --version, which identifies it among same-named tools
# from other packages and catches a tag that disagrees with Cargo.toml. On
# any failure nothing is printed on stdout and the tarball is removed.
# tests/scripts/test-package-cli.sh holds this script to all of that.
set -euo pipefail

version="${1:-}"
label="${2:-}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="${3:-$root/target/release}"

# <cargo target>:<public name>, one per shipped tool.
tools=(mkfs_ext4:mkfs.ext4)
licences=(LICENSE)

die() { echo "package-cli: $*" >&2; exit 1; }

[ -n "$version" ] || die "usage: package-cli.sh <version> <label> [target-dir]"
[ -n "$label" ] || die "usage: package-cli.sh <version> <label> [target-dir]"

crate="$(sed -n 's/^name = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)"
[ -n "$crate" ] || die "no package name in $root/Cargo.toml"
repo="$(sed -n 's/^repository = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)"
repo="${repo%/}"
repo="${repo##*/}"
[ -n "$repo" ] || die "no repository in $root/Cargo.toml"

tarball="$crate-$version-$label.tar.gz"
work="$(mktemp -d)"

# ON ANY FAILURE, NO TARBALL: not a partial one, and not one a previous run
# left under the same name, which a caller could otherwise take for this
# run's output.
cleanup() {
    local status=$?
    rm -rf "$work"
    [ "$status" -eq 0 ] || rm -f "$tarball"
    return "$status"
}
trap cleanup EXIT

stage="$work/stage"
mkdir -p "$stage/bin" "$stage/share/$repo" "$work/unpacked"
want=()
for t in "${tools[@]}"; do
    built="$target_dir/${t%%:*}"
    [ -x "$built" ] || die "no built ${t%%:*} at $built (cargo build --release --locked --bin ${t%%:*})"
    cp "$built" "$stage/bin/${t#*:}"
    chmod 755 "$stage/bin/${t#*:}"
    want+=("bin/${t#*:}")
done
cp "$root/packaging/CAVEATS" "$stage/share/$repo/CAVEATS"
want+=("share/$repo/CAVEATS")
for f in "${licences[@]}"; do
    cp "$root/$f" "$stage/$f"
    want+=("$f")
done

# COPYFILE_DISABLE keeps macOS tar from adding ._ AppleDouble members.
COPYFILE_DISABLE=1 tar -czf "$tarball" -C "$stage" bin share "${licences[@]}"

# Files only: whether a tar lists the directories themselves varies by tar.
want_list="$(printf '%s\n' "${want[@]}" | sort)"
got_list="$(tar -tzf "$tarball" | sed 's|^\./||' | grep -v '/$' | sort)"
[ "$got_list" = "$want_list" ] \
    || die "$tarball holds [$(echo $got_list)], expected [$(echo $want_list)]"

tar -xzf "$tarball" -C "$work/unpacked"
[ -s "$work/unpacked/share/$repo/CAVEATS" ] || die "share/$repo/CAVEATS is empty"
for t in "${tools[@]}"; do
    tool="${t#*:}"
    exe="$work/unpacked/bin/$tool"
    [ -x "$exe" ] || die "bin/$tool is not executable in $tarball"
    "$exe" --help > /dev/null || die "$tool --help failed"
    reported="$("$exe" --version)" || die "$tool --version failed"
    [ "$reported" = "$tool ($crate) $version" ] \
        || die "$tool --version says '$reported', expected '$tool ($crate) $version'"
done

printf '%s\n' "$tarball"
