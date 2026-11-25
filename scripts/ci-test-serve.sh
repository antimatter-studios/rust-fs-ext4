#!/usr/bin/env bash
set -euo pipefail

[[ "${1:-}" == serve && "${3:-}" == tcg && "${4:-}" == full && "${5:-}" == -- && "$#" -gt 5 ]] || {
    echo 'Expected serve <arch> tcg full -- <command>' >&2
    exit 2
}
arch="$2"
shift 5
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
exec bash "$repo/scripts/ci-qemu-runner.sh" serve "$arch" tcg full -- \
    bash "$repo/scripts/ci-test-run-prebuilt.sh" "$arch" "${EXT4_CI_BUNDLE_KEY:?bundle key required}" "$@"
