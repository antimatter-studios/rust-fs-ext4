#!/usr/bin/env bash
#
# ci-test-harness-prebuilt.sh <arch> <key> <command...> — run a command
# against the build job's prebuilt test bundle on this runner, with the
# tests' tools and kernel in the HARNESS's guest.
#
# The test job of a build-then-test pipeline. The build job compiled the
# executables in the test builder container (scripts/ci-test-compile.sh),
# at /opt/ext4-ci, the path Cargo compiled into them; this installs them
# there and runs the command with FS_EXT4_PREBUILT_REPO set, so
# scripts/test.sh runs them and compiles nothing. Every tool call and kernel
# mount goes to the guest fs-linux-test-harness boots (vm.sh), with the
# accelerator and memory limits the job sets (FLTH_ACCELERATOR,
# FLTH_VM_MEM, FLTH_GUEST_MEM) -- not to a served pilot guest.
set -euo pipefail

arch="${1:?bundle architecture required}"
key="${2:?bundle key required}"
shift 2
[[ "$key" =~ ^[0-9a-f]{64}$ && "$#" -gt 0 ]] || {
    echo 'Expected a bundle key and a test command' >&2
    exit 2
}
[ -z "${FS_EXT4_GUEST_SSH:-}" ] || {
    echo 'ci-test-harness-prebuilt.sh runs against the harness guest; FS_EXT4_GUEST_SSH names a served one.' >&2
    exit 1
}

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
bundle="$repo/tmp/ci-test-bundle/bundle.tar"
test -r "$bundle" || { echo "Missing prebuilt test bundle: $bundle" >&2; exit 1; }

started="$(date +%s)"
cd "$repo"
if ! mkdir /opt/ext4-ci 2>/dev/null; then
    sudo install -d -o "$(id -u)" -g "$(id -g)" /opt/ext4-ci
fi
bash scripts/ci-test-workspace.sh
cd /opt/ext4-ci/rust-fs-ext4
ln -s "$repo/tmp" tmp
python3 scripts/ci-test-bundle.py install "$arch" "$key" "$bundle"
printf 'Prebuilt %s bundle restored in %ss\n' "$arch" "$(( $(date +%s) - started ))"
export FS_EXT4_PREBUILT_REPO="$PWD"
exec "$@"
