#!/usr/bin/env bash
#
# ci-harness-guest-test.sh <arch> <key> [guest-suite args...] — CI's test job
# for an architecture the runner cannot run: hand the build job's bundle to
# fs-linux-test-harness as its build, and run scripts/guest-suite.sh in the
# harness's guest (FLTH_GUEST_ARCH, FLTH_ACCELERATOR=tcg) on it.
#
# The same chain as a workstation's `chore test:vm`: the guest compiles
# nothing (FLTH_BUILD_PREBUILT=1), and the siblings come from their pinned
# checkouts' HEAD, staged on the share as `chore test:vm` stages them.
set -euo pipefail

arch="${1:?bundle architecture required}"
key="${2:?bundle key required}"
shift 2
[[ "$key" =~ ^[0-9a-f]{64}$ ]] || { echo 'Expected a SHA-256 bundle key' >&2; exit 2; }
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
harness="$repo/../fs-linux-test-harness/scripts/vm.sh"
bundle="$repo/tmp/ci-test-bundle/bundle.tar"
test -r "$bundle" || { echo "Missing prebuilt test bundle: $bundle" >&2; exit 1; }

cd "$repo"
share="$("$harness" share)"
rm -rf "$share/build"
mkdir -p "$share/build"
mv "$bundle" "$share/build/bundle.tar"
printf '%s\n' "$key" >"$share/build/key"
printf 'ci\n' >"$share/build/scope"
for sibling in rust-fs-core fs-linux-test-harness; do
    rm -rf "$share/siblings/$sibling"
    mkdir -p "$share/siblings/$sibling"
    git -C "../$sibling" archive --format=tar HEAD | tar -x -C "$share/siblings/$sibling"
done
export FLTH_GUEST_ARCH="$arch" FLTH_BUILD_PREBUILT=1
exec "$harness" guest-test "$@"
