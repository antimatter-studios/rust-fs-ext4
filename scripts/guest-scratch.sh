#!/usr/bin/env bash
set -euo pipefail

[ "${FLTH_GUEST:-}" = 1 ] ||
    { echo "guest-scratch.sh runs INSIDE the harness VM." >&2; exit 1; }
[ "$#" -gt 0 ] || { echo "guest-scratch.sh needs a command." >&2; exit 2; }

repo="$(pwd -P)"
scratch="$(mktemp -d /var/tmp/fs-ext4-tests.XXXXXX)"
mounted=0

cleanup() {
    status=$?
    trap - EXIT
    if [ "$mounted" = 1 ] && ! umount "$repo/tmp"; then
        echo "guest-scratch.sh: could not unmount $repo/tmp; scratch retained at $scratch" >&2
        [ "$status" -ne 0 ] || status=1
    else
        rmdir "$scratch" 2>/dev/null ||
            echo "guest-scratch.sh: scratch retained at $scratch" >&2
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$repo/tmp"
mount --bind "$scratch" "$repo/tmp"
mounted=1
"$@"
