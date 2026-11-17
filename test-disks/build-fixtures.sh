#!/usr/bin/env bash
#
# build-fixtures.sh [image...]  build the test-disks/*.img fixtures
#                               (`chore fixtures`); name some to rebuild
#                               only those, e.g. `build-fixtures.sh htree xattr`
# build-fixtures.sh --check     exit 1 naming every fixture that is missing
#
# THE FIXTURE LIST lives here (IMAGES below) and in chores.yml's
# `fixtures` generates:, which must name the same files.
#
# Every fixture here needs the real kernel's ext4 driver to populate it,
# so the work happens in the fs-linux-test-harness VM (the sibling
# checkout at ../fs-linux-test-harness, moved to its pinned ref by
# `chore siblings`): test-disks/guest-build-images.sh runs as root in the
# guest, writes finished images into the shared directory, and this
# script moves them into test-disks/ on the host.
#
# The run on the shared guest ends when this script exits; the guest stays
# up for the next run.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"

# builder target -> image file, in build order.
IMAGES="basic:ext4-basic.img htree:ext4-htree.img csum_seed:ext4-csum-seed.img
no_csum:ext4-no-csum.img deep_extents:ext4-deep-extents.img inline:ext4-inline.img
xattr:ext4-xattr.img acl:ext4-acl.img largedir:ext4-largedir.img
manyfiles:ext4-manyfiles.img whole_disk:ext4-whole-disk.img"

if [ "${1:-}" = "--check" ]; then
    gone=""
    for pair in $IMAGES; do
        [ -f "$REPO/test-disks/${pair#*:}" ] || gone="$gone ${pair#*:}"
    done
    if [ -n "$gone" ]; then
        echo "fixtures missing from test-disks/:$gone" >&2
        echo "build them with 'chore fixtures' — tests never skip on a missing fixture." >&2
        exit 1
    fi
    echo "fixtures: all $(echo $IMAGES | wc -w) present in test-disks/"
    exit 0
fi

targets="$*"
if [ -z "$targets" ]; then
    for pair in $IMAGES; do targets="$targets ${pair%%:*}"; done
fi

HARNESS="$REPO/../fs-linux-test-harness"
VM="$HARNESS/scripts/vm.sh"

if [ ! -x "$VM" ]; then
    echo "build-fixtures: the harness is not checked out at $HARNESS." >&2
    echo "                Run 'chore siblings' first." >&2
    exit 1
fi

cd "$REPO"
# shellcheck source=/dev/null
. "$HARNESS/scripts/vm-session.sh"

share="$("$VM" share)"
out="$share/fixtures"
rm -rf "$out"
mkdir -p "$out"
cp test-disks/guest-build-images.sh "$share/guest-build-images.sh"

started=$(date +%s)
"$VM" run "bash /share/guest-build-images.sh /share/fixtures $targets"

built=0
for img in "$out"/*.img; do
    [ -e "$img" ] || continue
    # Checked on the host rather than taken on the guest's word: every
    # image must carry the ext4 superblock magic (0xEF53 at byte 1080 of
    # the filesystem; the whole-disk image's filesystem starts at its
    # partition, 2048 sectors in).
    base="$(basename "$img")"
    offset=1080
    [ "$base" = ext4-whole-disk.img ] && offset=$((2048 * 512 + 1080))
    magic="$(od -An -tx1 -j"$offset" -N2 "$img" | tr -d ' \n')"
    if [ "$magic" != 53ef ]; then
        echo "build-fixtures: $base has no ext4 superblock magic (got '$magic')" >&2
        exit 1
    fi
    cp --sparse=always "$img" "test-disks/$base.partial"
    mv -f "test-disks/$base.partial" "test-disks/$base"
    rm -f "$img"
    built=$((built + 1))
done
[ "$built" -gt 0 ] || { echo "build-fixtures: the guest produced no images" >&2; exit 1; }
echo "build-fixtures: $built image(s) in test-disks/ ($(( $(date +%s) - started ))s in the VM)"
