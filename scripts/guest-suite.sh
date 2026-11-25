#!/usr/bin/env bash
#
# guest-suite.sh [cargo test args...] — THE WHOLE SUITE, RUN INSIDE THE VM,
# FROM WHAT THE HOST BUILT.
#
# The fs-linux-test-harness [test] guest_command. `chore test:vm` (and `chore
# test` on a host that is not Linux) is a chain, as the pipeline is: the
# harness's [build] step compiles the test executables in the test builder
# container on the host (scripts/test-bundle-build.sh local, in the image CI
# compiles in), then boots the guest and runs this from /repo. This installs
# that bundle and runs it. Nothing is compiled here, and the guest has no
# Rust toolchain.
#
# WHY IT EXISTS. We run the Linux tests on Linux. On a Linux host that is
# the host itself and this is not used. On a Mac there is no ext4, no
# e2fsprogs worth trusting and no loop mount, so the suite runs in here.
#
# THE EXECUTABLES RUN FROM /opt/ext4-ci, the path they were built at
# (scripts/ci-test-bundle.py): Cargo compiles the checkout's path into every
# test. Each run gets a private one, bound over that path in the run's own
# mount namespace, so two runs on the shared guest never share it. The
# repository is copied there from /repo as it is checked out, and the
# siblings from the share, where `chore test:vm` staged their pinned HEADs.
# guest-scratch.sh binds a fresh guest-disk directory over tmp/ for the
# tests: share-backed hard links and FIFOs do not have the same semantics on
# every host (#613).
set -euo pipefail

[ "${FLTH_GUEST:-}" = 1 ] ||
    { echo "guest-suite.sh runs INSIDE the harness VM ('chore test:vm')." >&2; exit 1; }

build="${FLTH_BUILD_DIR:-}"
[ -n "$build" ] && [ -f "$build/bundle.tar" ] && [ -f "$build/key" ] || {
    echo "guest-suite.sh: no build at ${build:-FLTH_BUILD_DIR (unset)}." >&2
    echo "    'chore test:vm' builds the suite in a container before it boots the guest;" >&2
    echo "    the guest never compiles it." >&2
    exit 1
}
key="$(cat "$build/key")"
arch="$(uname -m)"

# The sibling checkouts, by directory name. The harness mounts us at /repo,
# whose parent is the guest's root, so ../<sibling> is /<sibling>: the task
# stages each on the share and this links it into place.
SIBLINGS="rust-fs-core fs-linux-test-harness"
for sibling in $SIBLINGS; do
    staged="/share/siblings/$sibling"
    [ -d "$staged" ] || {
        echo "guest-suite.sh: $sibling is not staged on the share; 'chore test:vm' does that." >&2
        exit 1
    }
    [ -L "/$sibling" ] || ln -sfn "$staged" "/$sibling"
done

base=/opt/ext4-ci
mkdir -p "$base"
private="$(mktemp -d /var/tmp/ext4-ci.XXXXXX)"
mount --bind "$private" "$base"
copy_tree() {
    mkdir -p "$base/$2"
    tar -C "$1/" -cf - --exclude=./.git --exclude=./target --exclude=./tmp \
        --exclude=./.vm-share --exclude=./.vagrant . | tar -xf - -C "$base/$2"
}
copy_tree /repo rust-fs-ext4
for sibling in $SIBLINGS; do
    copy_tree "/$sibling" "$sibling"
done

cd "$base/rust-fs-ext4"
python3 scripts/ci-test-bundle.py install "$arch" "$key" "$build/bundle.tar" local
export FS_EXT4_PREBUILT_REPO="$PWD"
# What tests/ci_profile.rs would ask Cargo, recorded by the build.
export FS_EXT4_CARGO_METADATA="$build/cargo-metadata.json"

echo "== in-guest suite: $(uname -srm), the $arch bundle built on the host"
started=$(date +%s)
bash scripts/guest-scratch.sh scripts/test.sh --locked --profile test-opt "$@"
echo "== in-guest suite: $(( $(date +%s) - started ))s"
