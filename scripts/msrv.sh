#!/usr/bin/env bash
#
# msrv.sh — build the library and the binaries on the toolchain Cargo.toml's
# `rust-version` names, and on nothing newer.
#
# The version is read from Cargo.toml, not repeated here or in CI, so the
# declaration and the check cannot drift apart. `+<version>` overrides
# rust-toolchain.toml, which pins the (newer) toolchain everything else uses.
#
# Only the library and the binaries: they are what a consumer builds. The
# binaries include the command-line tools, which need the `cli` feature. The
# dev-dependencies need a newer compiler and never reach one.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$REPO"

version="$(sed -n 's/^rust-version *= *"\([0-9.]*\)".*/\1/p' Cargo.toml | head -n 1)"
if [[ -z "$version" ]]; then
    echo "msrv: Cargo.toml declares no rust-version" >&2
    exit 1
fi

rustup toolchain install "$version" --profile minimal --no-self-update >/dev/null 2>&1 \
    || { echo "msrv: could not install Rust $version" >&2; exit 1; }

# Its own target directory: artefacts from the pinned toolchain are not
# reusable by this one, and sharing would rebuild both every time.
CARGO_TARGET_DIR="$REPO/target/msrv" cargo "+$version" check --locked --lib --bins --features cli --quiet
echo "msrv: builds on Rust $version (Cargo.toml rust-version)"
