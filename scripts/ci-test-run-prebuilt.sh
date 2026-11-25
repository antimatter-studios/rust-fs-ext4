#!/usr/bin/env bash
set -euo pipefail

arch="${1:?bundle architecture required}"
key="${2:?bundle key required}"
shift 2
[[ "${FS_EXT4_GUEST_ARCH:-}" == "$arch" && -n "${FS_EXT4_GUEST_SSH:-}" ]] || {
    echo 'Prebuilt workloads require the matching served QEMU guest' >&2
    exit 1
}
[[ "$key" =~ ^[0-9a-f]{64}$ && "$#" -gt 0 ]] || {
    echo 'Expected a bundle key and a test command' >&2
    exit 2
}

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
bundle="$repo/tmp/ci-test-bundle/bundle.tar"
test -r "$bundle" || { echo "Missing prebuilt test bundle: $bundle" >&2; exit 1; }

restore_started="$(date +%s)"
cd "$repo"
mkdir /opt/ext4-ci
bash scripts/ci-test-workspace.sh
cd /opt/ext4-ci/rust-fs-ext4
ln -s "$repo/tmp" tmp
python3 scripts/ci-test-bundle.py install "$arch" "$key" "$bundle"
restore_seconds="$(( $(date +%s) - restore_started ))"
printf '%s\n' "$restore_seconds" >"${FS_EXT4_GUEST_SSH%/*}/bundle-restore-seconds.txt"
printf 'Prebuilt %s bundle restored in %ss\n' "$arch" "$restore_seconds"
export FS_EXT4_PREBUILT_REPO="$PWD"
exec "$@"
