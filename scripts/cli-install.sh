#!/usr/bin/env bash
# cli-install.sh                   build the command-line tools and stage them
# cli-install.sh --print-bin-dir   print where they are staged, and exit
#
# Builds the multi-call binary in release mode (`--features cli`, which the
# target requires) and stages it in tmp/cli/bin under every name it answers
# to: `rust-fs-ext4`, the real file, and each dotted name as a relative
# symlink to it -- the layout an install has. The dotted names come from the
# binary itself (`rust-fs-ext4 generate names`), so this script names none.
#
# It prints the PATH line to use. `chore test:cli` tests whatever PATH
# finds, so the staged directory has to come first for it to test these.
#
# CLI_INSTALL_DIR names another prefix (the binaries go in its bin/).
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
PREFIX="${CLI_INSTALL_DIR:-$REPO/tmp/cli}"
BIN="$PREFIX/bin"

if [ "${1:-}" = "--print-bin-dir" ]; then
    printf '%s\n' "$BIN"
    exit 0
fi

cd "$REPO"
cargo build --locked --release --features cli --bin rust-fs-ext4 --quiet
built="${CARGO_TARGET_DIR:-$REPO/target}/release/rust-fs-ext4"
[ -x "$built" ] || { echo "cli-install: cargo built no $built" >&2; exit 1; }

rm -rf "$BIN"
mkdir -p "$BIN"
cp "$built" "$BIN/rust-fs-ext4"
names="$("$BIN/rust-fs-ext4" generate names)"
[ -n "$names" ] || { echo "cli-install: the binary lists no tool names" >&2; exit 1; }
for name in $names; do
    ln -s rust-fs-ext4 "$BIN/$name"
done

echo "cli:install: staged rust-fs-ext4 and $(echo $names | tr ' ' ',') in $BIN"
echo "cli:install: to test or use them, put that directory first on PATH:"
echo "  export PATH=\"$BIN:\$PATH\""
