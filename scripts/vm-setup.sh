#!/usr/bin/env bash
#
# vm-setup.sh — the fs-linux-test-harness [setup] script. Runs as root
# INSIDE the VM, re-applied by the harness whenever this file changes.
#
# THE GUEST IS WHERE THE ORACLE TOOLS LIVE. Not the host: e2fsprogs on a
# workstation is whatever that machine has — a keg-only Homebrew formula
# on a Mac, a distribution build on Linux, a different version per
# developer — and on a Mac it is not even the platform these images are
# for. One Debian guest, one version, the same answers for everyone.
#
#   e2fsprogs   mke2fs, mkfs.ext4, e2fsck, fsck.ext4, debugfs, dumpe2fs,
#               tune2fs — the oracle tools (tests/support/src/oracle.rs)
#               and the fixture builder's formatter
#   attr, acl   setfattr/getfattr, setfacl/getfacl: what the kernel
#               oracle reads back (tests/support/src/kernel.rs)
#   fdisk       sfdisk, for the whole-disk fixture's GPT
#   util-linux  losetup and mount: the kernel oracle's loop mounts, which
#               happen here and nowhere else
#   lwext4      A THIRD IMPLEMENTATION OF EXT4, built here from source at
#               a pinned commit. e2fsprogs and this crate read the same
#               specification and inherit the same ambiguities; Linux is
#               the thing the images are for. lwext4 (BSD-2-Clause, pure
#               C, github.com/gkostka/lwext4) shares a lineage with
#               neither, so where it disagrees with us one of the two has
#               the format wrong -- which is what
#               tests/lwext4_cross_validate.rs is for. It is a portable C
#               library, so it needs a machine and not a BSD: this guest
#               already is that machine (#269)
#
# AND WHAT A RUST BUILD NEEDS FROM THE DISTRIBUTION (curl, gcc, libc6-dev,
# pkg-config), for `chore test:vm`: the whole suite compiled and run in
# here, which is how a macOS host runs a Linux test suite at all. NOT THE
# TOOLCHAIN ITSELF: scripts/guest-suite.sh installs that through
# `../rust-fs-core/scripts/guest-rust-toolchain.sh`, rust-fs-core's one copy of the
# install, which every driver runs and which recovers from an install a
# reaper or a deadline interrupted. It cannot run from here: the harness
# ships this one file into the guest, before `test:vm` has staged the core
# sibling on the share (rust-fs-core#190).
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

REPO=/repo

# THE lwext4 PIN. A commit, not a branch: the family pins every sibling,
# every box and every toolchain, and an oracle that moves on its own is
# an oracle whose verdict cannot be compared with yesterday's. Bump it
# deliberately, and expect to re-measure the tier when you do.
#
# 58bcf89 is master as of 2022-09-22, the newest revision upstream has.
# tests/support/src/lwext4.rs holds the same string and refuses to run
# against a guest built from another one, so the two cannot drift.
LWEXT4_PIN=58bcf89a121b72d4fb66334f1693d3b30e4cb9c5
LWEXT4_REPO=https://github.com/gkostka/lwext4.git

# ONE PATCH ON TOP OF THE PIN (#272). At the pin, lwext4's file read takes an
# unmapped block in the body of a file as block 0 of the device rather
# than as zeros, so every sparse file came back with the volume's first
# blocks in its holes (reported upstream as gkostka/lwext4#101; the pin
# is still upstream's newest commit). tests/lwext4/fread-holes.patch
# zero-fills a hole, as the function already does for a hole in an
# unaligned leading block, and nothing else. Its digest is recorded here
# because the harness re-runs this script only when THIS FILE changes:
# a patch edited without updating the digest fails setup, and updating
# the digest is what makes the next boot rebuild. The stamp holds both,
# and tests/support/src/lwext4.rs refuses a guest built from anything
# else.
LWEXT4_PATCH="$REPO/tests/lwext4/fread-holes.patch"
LWEXT4_PATCH_SHA256=7bfe03b34cb24cbd4b1f8ed5892664d01fcf6dc226ec95e80778642d9c597201
LWEXT4_SRC=/var/lib/fs-ext4-lwext4
LWEXT4_PREFIX=/usr/local

apt-get update -qq
apt-get install -y -qq e2fsprogs attr acl fdisk util-linux curl gcc libc6-dev pkg-config \
    git cmake make >/dev/null
modprobe loop

# sed, not head: head exits after one line, mke2fs gets SIGPIPE writing
# its second, and pipefail turns that into a failed setup (seen on CI).
mkfs.ext4 -V 2>&1 | sed -n 1p

# 1.47.0 is the first e2fsprogs that knows `orphan_file`, which the
# oracle tests turn off by name (`-O ^orphan_file`); older releases
# refuse the option outright. Debian 12 ships exactly 1.47.0.
version="$(mke2fs -V 2>&1 | sed -n 's/^mke2fs \([0-9][0-9.]*\).*/\1/p' | head -1)"
if [ -z "$version" ] ||
    [ "$(printf '%s\n%s\n' 1.47.0 "$version" | sort -V | head -1)" != 1.47.0 ]; then
    echo "vm-setup: e2fsprogs ${version:-of unknown version} is older than 1.47.0" >&2
    exit 1
fi

# lwext4, at the pinned commit. Idempotent by the stamp, which holds the
# commit that was built: a changed pin rebuilds, an unchanged one costs a
# `test`. The harness re-runs this whole script whenever it changes
# (it stamps the script's own sha256 in the guest), so bumping the pin
# above is all it takes for the next boot to rebuild.
#
# A shallow fetch OF THE COMMIT, not a clone of the branch it is on: the
# pin may name a revision no branch tip points at, which `git clone
# --branch` cannot express.
patch_sha="$(sha256sum "$LWEXT4_PATCH" | cut -d' ' -f1)"
[ "$patch_sha" = "$LWEXT4_PATCH_SHA256" ] || {
    echo "vm-setup: $LWEXT4_PATCH has sha256 $patch_sha, not the $LWEXT4_PATCH_SHA256 recorded here; update LWEXT4_PATCH_SHA256" >&2
    exit 1
}
lwext4_build="$LWEXT4_PIN $LWEXT4_PATCH_SHA256"
lwext4_stamp="$LWEXT4_PREFIX/lib/lwext4.pin"
if [ "$(cat "$lwext4_stamp" 2>/dev/null || true)" != "$lwext4_build" ]; then
    echo "vm-setup: building lwext4 $LWEXT4_PIN with $(basename "$LWEXT4_PATCH")"
    if [ ! -d "$LWEXT4_SRC/.git" ]; then
        rm -rf "$LWEXT4_SRC"
        git init --quiet "$LWEXT4_SRC"
        git -C "$LWEXT4_SRC" remote add origin "$LWEXT4_REPO"
    fi
    git -C "$LWEXT4_SRC" fetch --quiet --depth 1 origin "$LWEXT4_PIN"
    git -C "$LWEXT4_SRC" checkout --quiet FETCH_HEAD
    [ "$(git -C "$LWEXT4_SRC" rev-parse HEAD)" = "$LWEXT4_PIN" ] || {
        echo "vm-setup: lwext4 is not at $LWEXT4_PIN" >&2
        exit 1
    }
    # From a clean tree, so a patch applied by an earlier build is not
    # applied twice.
    git -C "$LWEXT4_SRC" reset --quiet --hard
    git -C "$LWEXT4_SRC" apply "$LWEXT4_PATCH"

    # The `generic` flavour: host tooling, no embedded target. It is the
    # branch of lwext4's CMakeLists that sets BLOCKDEV_TYPE=linux, which
    # is what builds the file-backed block device the reporter opens an
    # image through.
    rm -rf "$LWEXT4_SRC/build"
    mkdir -p "$LWEXT4_SRC/build"
    (cd "$LWEXT4_SRC/build" && cmake -G "Unix Makefiles" \
        -DCMAKE_BUILD_TYPE=Release \
        -DVERSION_MAJOR=1 -DVERSION_MINOR=0 -DVERSION_PATCH=0 -DVERSION=1.0.0 \
        -DCMAKE_TOOLCHAIN_FILE=../toolchain/generic.cmake ..) >/dev/null
    make -C "$LWEXT4_SRC/build" -j"$(nproc)" >/dev/null

    # Installed by hand rather than by `make install`: lwext4's install
    # rules ship the generated config and the libraries but not the
    # public headers or the block device's, and the reporter needs all
    # four. include/ext4_config.h #includes generated/ext4_config.h
    # unconditionally, so the generated one must travel with it or the
    # program would compile against a different configuration than the
    # library was built with -- same structs, different layout.
    rm -rf "$LWEXT4_PREFIX/include/lwext4"
    mkdir -p "$LWEXT4_PREFIX/include/lwext4/generated" "$LWEXT4_PREFIX/lib"
    cp "$LWEXT4_SRC"/include/*.h "$LWEXT4_PREFIX/include/lwext4/"
    cp -r "$LWEXT4_SRC/include/misc" "$LWEXT4_PREFIX/include/lwext4/"
    cp "$LWEXT4_SRC/build/include/generated/ext4_config.h" \
        "$LWEXT4_PREFIX/include/lwext4/generated/"
    cp "$LWEXT4_SRC/blockdev/linux/file_dev.h" "$LWEXT4_PREFIX/include/lwext4/"
    cp "$LWEXT4_SRC/build/src/liblwext4.a" "$LWEXT4_SRC/build/blockdev/libblockdev.a" \
        "$LWEXT4_PREFIX/lib/"
    # LAST, so an interrupted build is not mistaken for a finished one.
    printf '%s\n' "$lwext4_build" > "$lwext4_stamp"
fi
echo "vm-setup: lwext4 $(cat "$lwext4_stamp")"

echo "vm-setup: the oracle tools and lwext4 are installed in the guest"
