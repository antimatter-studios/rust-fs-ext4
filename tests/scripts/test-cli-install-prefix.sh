#!/usr/bin/env bash
# scripts/cli-install.sh installs into a prefix without clearing it.
#
# CLI_INSTALL_DIR may name a prefix that is not ours alone ($HOME/.local),
# so the installer replaces only rust-fs-ext4 and the links it owns, keeps
# every other file in bin/, and refuses a file under one of its names that
# it did not put there. cargo is a stub here that "builds" a script
# answering `generate names`, so this runs in a second with no toolchain.
#
#   bash tests/scripts/test-cli-install-prefix.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SANDBOX="$(mktemp -d)"
trap 'rm -rf "$SANDBOX"' EXIT
fails=0
fail() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

mkdir -p "$SANDBOX/stub" "$SANDBOX/target/release" "$SANDBOX/prefix/bin"
cat >"$SANDBOX/stub/cargo" <<'STUB'
#!/usr/bin/env bash
out="$CARGO_TARGET_DIR/release/rust-fs-ext4"
printf '#!/bin/sh\n[ "$1 $2" = "generate names" ] && printf "mkfs.ext4\\nfs.ext4\\n"\nexit 0\n' >"$out"
chmod +x "$out"
STUB
chmod +x "$SANDBOX/stub/cargo"
echo keep >"$SANDBOX/prefix/bin/unrelated"

install() {
    PATH="$SANDBOX/stub:$PATH" CARGO_TARGET_DIR="$SANDBOX/target" \
        CLI_INSTALL_DIR="$SANDBOX/prefix" bash "$REPO/scripts/cli-install.sh" >"$SANDBOX/out" 2>&1
}

install || fail "the first install failed: $(cat "$SANDBOX/out")"
[[ "$(cat "$SANDBOX/prefix/bin/unrelated" 2>/dev/null)" == keep ]] ||
    fail "an unrelated file in the prefix's bin/ was removed"
[[ "$(readlink "$SANDBOX/prefix/bin/fs.ext4")" == rust-fs-ext4 ]] ||
    fail "fs.ext4 is not a link to rust-fs-ext4"
install || fail "a second install over the first failed: $(cat "$SANDBOX/out")"
rm "$SANDBOX/prefix/bin/mkfs.ext4"
echo theirs >"$SANDBOX/prefix/bin/mkfs.ext4"
if install; then fail "a mkfs.ext4 the installer did not make was overwritten"; fi
[[ "$(cat "$SANDBOX/prefix/bin/mkfs.ext4")" == theirs ]] ||
    fail "the refused install changed the file it refused"

if ((fails > 0)); then exit 1; fi
echo "PASS  cli-install keeps what else is in its prefix"
