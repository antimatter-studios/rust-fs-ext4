#!/usr/bin/env bash
#
# wasm-tools.sh — what `chore test:wasm` needs (`chore tools:wasm`).
#
#   wasm-tools.sh           install what is missing, then verify
#   wasm-tools.sh --check   verify only; exit 1 naming what is missing
#
# Three things, none of which the other tiers need, which is why this is
# not part of scripts/tools.sh:
#
#   the target        wasm32-unknown-unknown's std, for the toolchain
#                     rust-toolchain.toml pins
#   the test runner   wasm-bindgen-test-runner, from wasm-bindgen-cli. It
#                     must be EXACTLY the wasm-bindgen version Cargo.lock
#                     resolved: the two halves of the bindings are generated
#                     separately and a mismatch is refused at run time. So
#                     the version is read from the lockfile, never written
#                     down twice.
#   node              the runner executes the tests in Node
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
TARGET=wasm32-unknown-unknown

MODE=install
[ "${1:-}" = "--check" ] && MODE=check

want="$(awk '
    $0 == "name = \"wasm-bindgen\"" { found = 1; next }
    found && /^version = / { gsub(/"/, "", $3); print $3; exit }
' "$REPO/Cargo.lock")"
if [ -z "$want" ]; then
    echo "wasm-tools: Cargo.lock resolves no wasm-bindgen; nothing to match the runner to." >&2
    exit 1
fi

have_target() { (cd "$REPO" && rustup target list --installed) | grep -qx "$TARGET"; }
have_runner() {
    command -v wasm-bindgen-test-runner >/dev/null 2>&1 &&
        [ "$(wasm-bindgen-test-runner --version 2>/dev/null | awk '{ print $2 }')" = "$want" ]
}

if [ "$MODE" = install ]; then
    # The first cargo call in the tree installs the toolchain rust-toolchain.toml
    # pins, so the target below is added to that toolchain and not another.
    (cd "$REPO" && cargo --version >/dev/null)
    have_target || (cd "$REPO" && rustup target add "$TARGET")
    have_runner || cargo install --quiet wasm-bindgen-cli --version "$want" --locked
fi

missing=0
if ! have_target; then
    echo "wasm-tools: the $TARGET target is not installed -- run 'chore tools:wasm'." >&2
    missing=1
fi
if ! have_runner; then
    echo "wasm-tools: wasm-bindgen-test-runner $want (Cargo.lock's wasm-bindgen) is not on PATH" >&2
    echo "            -- run 'chore tools:wasm'." >&2
    missing=1
fi
if ! command -v node >/dev/null 2>&1; then
    echo "wasm-tools: node is not on PATH; the wasm tests run in Node. Install Node.js." >&2
    missing=1
fi
[ "$missing" = 0 ] || exit 1
echo "wasm-tools: $TARGET, wasm-bindgen-test-runner $want, node $(node --version)"
