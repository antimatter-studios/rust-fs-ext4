#!/usr/bin/env bash
#
# ci-guest-suite.sh [cargo test args...] — `chore test:vm` on a CI runner
# without KVM: the suite the harness's [build] step compiled on this runner,
# RUN INSIDE the guest that `scripts/ci-qemu-runner.sh serve` booted beside
# this command.
#
# The same chain as a workstation's `chore test:vm`. The workflow runs the
# harness's [build] step first (`vm.sh build`, in the test builder container),
# which leaves the bundle in .vm-share/build. The harness gives its guest
# this repository at /repo, its share at /share, and the pinned siblings on
# that share; this does the same over ssh, then runs scripts/guest-suite.sh,
# as root with FLTH_GUEST=1 and FLTH_BUILD_DIR=/share/build. Nothing is
# compiled in the guest, and it has no Rust toolchain.
#
# The siblings come from their pinned checkouts' HEAD, as `chore test:vm`
# stages them, so the guest runs nothing uncommitted of theirs; this
# repository goes as it is checked out, with its fixtures and without .git,
# target/, tmp/ and the share.
set -euo pipefail

config="${FS_EXT4_GUEST_SSH:-}"
[ -n "$config" ] && [ -f "$config" ] || {
    echo "ci-guest-suite.sh: FS_EXT4_GUEST_SSH names no guest; run this under 'scripts/ci-qemu-runner.sh serve'." >&2
    exit 1
}
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
BUILD="$REPO/.vm-share/build"
[ -f "$BUILD/bundle.tar" ] && [ -f "$BUILD/key" ] || {
    echo "ci-guest-suite.sh: no build at $BUILD." >&2
    echo "    Run the harness's [build] step first: ../fs-linux-test-harness/scripts/vm.sh build" >&2
    exit 1
}
# The resource profile's test limits, which `serve` read from the harness:
# the in-guest tests run with as many threads as it allows, not one per
# guest CPU.
limits=""
for name in FLTH_TEST_JOBS RUST_TEST_THREADS; do
    [[ "${!name:-}" =~ ^[1-9][0-9]*$ ]] || {
        echo "ci-guest-suite.sh: $name is not set; 'scripts/ci-qemu-runner.sh serve' sets it from the resource profile." >&2
        exit 1
    }
    limits+=" $name=${!name}"
done
ssh_guest() { ssh -F "$config" guest "$@"; }

SIBLINGS="rust-fs-core fs-linux-test-harness"
ssh_guest 'rm -rf /repo /share/siblings /share/build && mkdir -p /repo /share/siblings /share/build' </dev/null
tar --sparse -cf - -C "$REPO" --exclude=./.git --exclude=./target --exclude=./tmp \
    --exclude=./.vm-share --exclude=./.vagrant . | ssh_guest 'tar -xf - -C /repo'
for sibling in $SIBLINGS; do
    [ -e "$REPO/../$sibling/.git" ] || {
        echo "ci-guest-suite.sh: ../$sibling is not checked out -- run 'chore siblings'." >&2
        exit 1
    }
    ssh_guest "rm -rf /$sibling" </dev/null
    git -C "$REPO/../$sibling" archive --format=tar HEAD |
        ssh_guest "mkdir -p /share/siblings/$sibling && tar -xf - -C /share/siblings/$sibling"
done
tar -cf - -C "$BUILD" . | ssh_guest 'tar -xf - -C /share/build'

command="cd /repo && FLTH_GUEST=1 FLTH_BUILD_DIR=/share/build$limits exec bash scripts/guest-suite.sh"
for word in "$@"; do command+=" $(printf %q "$word")"; done
exec ssh -n -F "$config" guest "$command"
