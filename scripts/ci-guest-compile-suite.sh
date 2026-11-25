#!/usr/bin/env bash
#
# ci-guest-compile-suite.sh [cargo test args...] — THE WHOLE SUITE, COMPILED
# AND RUN INSIDE THE GUEST that `scripts/ci-qemu-runner.sh serve` booted, for
# CI's suite-in-vm job (scripts/ci-guest-suite.sh).
#
# The harness's own guest no longer compiles: `chore test:vm` builds in the
# test builder container on the host and the guest runs what it built
# (scripts/guest-suite.sh). This is the one path left that compiles in a
# guest, until CI's suite-in-vm job runs that chain too.
#
# The toolchain and the build directory live on the VM's own disk
# (/var/lib, /var/cache). The repository is shared, but guest-scratch.sh
# binds a fresh guest-disk directory over tmp/ for the tests: share-backed
# hard links and FIFOs do not have the same semantics on every host (#613).
set -euo pipefail

[ "${FLTH_GUEST:-}" = 1 ] ||
    { echo "ci-guest-compile-suite.sh runs INSIDE the served CI guest (scripts/ci-guest-suite.sh)." >&2; exit 1; }

# The sibling checkouts the guest needs, by directory name.
SIBLINGS="rust-fs-core"

RUST_ROOT=/var/lib/fs-ext4-rust
export RUSTUP_HOME="$RUST_ROOT/rustup"
export CARGO_HOME="$RUST_ROOT/cargo"
export CARGO_TARGET_DIR=/var/cache/fs-ext4-target
export PATH="$CARGO_HOME/bin:$PATH"

# THE SIBLING CHECKOUTS. The suite runs core's scripts from
# ../rust-fs-core, which on the host is a sibling checkout -- and the
# guest is given this repository, not the directory that holds it. The
# harness mounts us at /repo, whose parent IS the guest's root, so
# `../rust-fs-core` resolves to /rust-fs-core: the task stages the
# sibling on the share (from its pinned, clean checkout) and this links
# it into place. `chore siblings` is what keeps it at the right ref.
# Cargo takes the crate from the registry; a local `[patch]` naming
# `../rust-fs-core` lands on this same link
# (docs/developing-against-core.md).
# shellcheck disable=SC2043  # one sibling today; the list is the point
for sibling in $SIBLINGS; do
    staged="/share/siblings/$sibling"
    [ -d "$staged" ] || {
        echo "ci-guest-compile-suite.sh: $sibling is not staged on the share; 'chore test:vm' does that." >&2
        exit 1
    }
    [ -L "/$sibling" ] || ln -sfn "$staged" "/$sibling"
done

cd /repo
# THE PINNED TOOLCHAIN, installed -- or repaired, when an earlier install
# was interrupted and left its wreckage on the VM's own disk. It is
# rust-fs-core's family script, the one copy every driver runs, reached
# through the sibling linked just above: this is the first point at which
# core is in the guest, which is why vm-setup.sh cannot do it. It reads
# rust-toolchain.toml and installs into RUSTUP_HOME and CARGO_HOME, and
# over a whole toolchain it costs one manifest check (rust-fs-core#190).
bash ../rust-fs-core/scripts/guest-rust-toolchain.sh
command -v cargo >/dev/null ||
    { echo "ci-guest-compile-suite.sh: no cargo in the guest after the toolchain install." >&2; exit 1; }

echo "== in-guest suite: $(uname -srm), $(cargo --version)"
started=$(date +%s)
bash scripts/guest-scratch.sh scripts/test.sh --locked --profile test-opt "$@"
echo "== in-guest suite: $(( $(date +%s) - started ))s"
