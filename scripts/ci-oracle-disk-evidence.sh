#!/usr/bin/env bash
set -euo pipefail

if [[ "${GITHUB_JOB:-}" != test || "${FS_EXT4_GUEST_ARCH:-}" != x86_64 || -z "${FS_EXT4_GUEST_SSH:-}" ]]; then
    [[ "${1:-}" != --heartbeat ]] || exit 0
    exec "$@"
fi

allocated_bytes() {
    local result blocks
    if [[ -n "$1" ]] && result="$(BLOCKSIZE=512 du -s -B512 -- "$1" 2>/dev/null)"; then
        blocks="${result%%$'\t'*}"
        if [[ "$blocks" =~ ^[0-9]+$ ]]; then
            printf '%s' "$((10#$blocks * 512))"
            return
        fi
    fi
    printf 'unavailable'
}

sample() {
    printf '[%s] oracle disk %s cargo_allocated_bytes=%s base_allocated_bytes=%s overlay_allocated_bytes=%s\n' \
        "$(date -u +%FT%TZ)" "$1" "$(allocated_bytes "${CARGO_TARGET_DIR:-target}")" \
        "$(allocated_bytes "${FS_EXT4_CI_VM_BASE:-}")" "$(allocated_bytes "${FS_EXT4_CI_VM_OVERLAY:-}")" |
        tee -a "${FS_EXT4_GUEST_SSH%/*}/oracle-allocated-bytes.log" || true
}

if [[ "${1:-}" == --heartbeat ]]; then
    sample heartbeat
    exit 0
fi

sample before
status=0
"$@" || status=$?
sample after
exit "$status"
