#!/usr/bin/env bash
#
# ci-guest-suite.sh [cargo test args...] — `chore test:vm` on a CI runner
# without KVM: the whole suite COMPILED AND RUN INSIDE the guest that
# `scripts/ci-qemu-runner.sh serve` booted beside this command.
#
# The harness gives its guest this repository at /repo and the pinned
# siblings on its share; this does the same over ssh, then runs the same
# scripts/guest-suite.sh the harness runs as its [test] guest_command, as
# root with FLTH_GUEST=1. The siblings come from their pinned checkouts'
# HEAD, as `chore test:vm` stages them, so the guest runs nothing
# uncommitted of theirs; this repository goes as it is checked out, with
# its fixtures and without .git, target/ and tmp/.
set -euo pipefail

config="${FS_EXT4_GUEST_SSH:-}"
[ -n "$config" ] && [ -f "$config" ] || {
    echo "ci-guest-suite.sh: FS_EXT4_GUEST_SSH names no guest; run this under 'scripts/ci-qemu-runner.sh serve'." >&2
    exit 1
}
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
# The resource profile's limits, which `serve` read from the harness: the
# in-guest build and test run with as many jobs and threads as it allows,
# not one per guest CPU, as the harness's own guest runs do.
limits=""
for name in CARGO_BUILD_JOBS FLTH_BUILD_JOBS FLTH_TEST_JOBS RUST_TEST_THREADS; do
    [[ "${!name:-}" =~ ^[1-9][0-9]*$ ]] || {
        echo "ci-guest-suite.sh: $name is not set; 'scripts/ci-qemu-runner.sh serve' sets it from the resource profile." >&2
        exit 1
    }
    limits+=" $name=${!name}"
done
ssh_guest() { ssh -F "$config" guest "$@"; }

ssh_guest 'rm -rf /repo /rust-fs-core /share/siblings && mkdir -p /repo /share/siblings' </dev/null
tar --sparse -cf - -C "$REPO" --exclude=./.git --exclude=./target --exclude=./tmp \
    --exclude=./.vm-share --exclude=./.vagrant . | ssh_guest 'tar -xf - -C /repo'
for sibling in rust-fs-core; do
    [ -e "$REPO/../$sibling/.git" ] || {
        echo "ci-guest-suite.sh: ../$sibling is not checked out -- run 'chore siblings'." >&2
        exit 1
    }
    git -C "$REPO/../$sibling" archive --format=tar HEAD |
        ssh_guest "mkdir -p /share/siblings/$sibling && tar -xf - -C /share/siblings/$sibling"
done

command="cd /repo && FLTH_GUEST=1$limits exec bash scripts/guest-suite.sh"
for word in "$@"; do command+=" $(printf %q "$word")"; done
exec ssh -n -F "$config" guest "$command"
