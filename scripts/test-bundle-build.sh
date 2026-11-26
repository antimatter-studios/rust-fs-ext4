#!/usr/bin/env bash
#
# test-bundle-build.sh — compile the Linux test executables into a bundle,
# INSIDE the test builder container. The first link of the chain: this
# compiles, the guest only runs what it compiled.
#
#   test-bundle-build.sh ci <arch> <key> <bundle> [--lint]
#       CI's compile job (scripts/ci-test-compile.sh): the tiers its test jobs
#       split between them, keyed by the inputs CI hashed.
#   test-bundle-build.sh local
#       The harness's [build] command (`chore test:vm`): the whole suite for
#       the guest's architecture, FLTH_GUEST_ARCH, written to FLTH_BUILD_OUTPUT
#       with the key the guest checks it by.
#
# Both copy the checkout to /opt/ext4-ci, the path the executables are built
# for and run from in the guest (scripts/ci-test-bundle.py).
set -euo pipefail

mode="${1:-}"
lint=0
case "$mode" in
    ci)
        arch="${2:?architecture required}"
        key="${3:?bundle key required}"
        bundle="${4:?bundle path required}"
        [ "${5:-}" != --lint ] || lint=1
        scope=ci ;;
    local)
        [ "${FLTH_BUILD:-}" = 1 ] || {
            echo "test-bundle-build.sh local is the harness's [build] command; run 'chore test:vm'." >&2
            exit 1
        }
        arch="${FLTH_GUEST_ARCH:?}"
        output="${FLTH_BUILD_OUTPUT:?}"
        bundle="$output/bundle.tar"
        # Nothing the host knows names this build, so the key is made here and
        # handed to the guest beside the bundle: it installs only this one.
        key="$(head -c 32 /dev/urandom | sha256sum | cut -d' ' -f1)"
        scope=local ;;
    *)
        echo 'usage: test-bundle-build.sh ci <arch> <key> <bundle> [--lint] | local' >&2
        exit 2 ;;
esac

mkdir /opt/ext4-ci
bash scripts/ci-test-workspace.sh
cd /opt/ext4-ci/rust-fs-ext4
# A checkout made by `git worktree add` has a .git FILE naming a directory on
# the host, which the container cannot see, and every git command, even
# `git config --global`, refuses to run beside it. The build needs nothing of
# git, and this copy is the container's own.
[ ! -f .git ] || rm .git
git config --global --add safe.directory "$PWD"

if [ "$scope" = local ] && [ -n "${FLTH_BUILD_CACHE:-}" ]; then
    # Kept between local builds (the harness's volume), so a second build is
    # incremental: the copy above keeps the sources' times, which is what Cargo
    # compares. The registry is kept too, so it is not fetched every time.
    mkdir -p "$FLTH_BUILD_CACHE/target" "$FLTH_BUILD_CACHE/registry"
    ln -s "$FLTH_BUILD_CACHE/target" target
    rm -rf "${CARGO_HOME:?}/registry"
    ln -s "$FLTH_BUILD_CACHE/registry" "$CARGO_HOME/registry"
fi

export CARGO_BUILD_TARGET="$arch-unknown-linux-gnu"
export CARGO_BUILD_JOBS="${FLTH_BUILD_JOBS:-${CARGO_BUILD_JOBS:-1}}"
if [ "$arch" != "$(uname -m)" ]; then
    [ "$arch" = aarch64 ] || {
        echo "test-bundle-build.sh: this builder cross-compiles only for aarch64, not $arch." >&2
        exit 1
    }
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
    export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
    export AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
fi
[ "$lint" = 0 ] || chore lint
python3 scripts/ci-test-bundle.py build "$arch" "$key" "$bundle" "$scope"

if [ "$scope" = local ]; then
    python3 scripts/ci-test-bundle.py verify "$arch" "$key" "$bundle" local
    printf '%s\n' "$key" > "$output/key"
fi
