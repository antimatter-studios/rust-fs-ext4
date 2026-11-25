#!/usr/bin/env bash
#
# test-builder-context.sh <dir> — write the test builder image's recipe into an
# empty directory: the Dockerfile, the pinned toolchain file, and rust-fs-core's
# toolchain installer. Everything `docker build` sees.
#
# One recipe, two callers. CI's image job (scripts/ci-test-builder-image.sh)
# keys its registry image by a SHA-256 of these files, and the harness's
# [build] step (fs-linux-test-harness.toml) builds its local image from the
# same files, so a workstation compiles in the image CI compiles in.
set -euo pipefail

dir="${1:?usage: test-builder-context.sh <empty directory>}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
core="$repo/../rust-fs-core/scripts/guest-rust-toolchain.sh"
[ -f "$core" ] || {
    echo "test-builder-context.sh: $core is missing -- run 'chore siblings'." >&2
    exit 1
}
cp "$repo/scripts/ci-test-builder/Dockerfile" "$repo/rust-toolchain.toml" "$dir/"
cp "$core" "$dir/core-toolchain.sh"
