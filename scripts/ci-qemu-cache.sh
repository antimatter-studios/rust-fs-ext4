#!/usr/bin/env bash
set -euo pipefail

operation="${1:?pull or push required}"
kind="${2:?cache kind required}"
arch="${3:?guest architecture required}"
key="${4:?content key required}"
directory="${5:?cache directory required}"
shift 5
registry="${BENCH_CACHE_REGISTRY:-}"
case "$operation/$kind/$arch" in pull/tests/x86_64|pull/tests/aarch64|pull/guest/x86_64|pull/guest/aarch64|push/tests/x86_64|push/tests/aarch64|push/guest/x86_64|push/guest/aarch64) ;; *) echo 'invalid OCI cache operation' >&2; exit 2 ;; esac
[[ "$key" =~ ^[0-9a-f]{64}$ ]] || { echo 'invalid OCI cache content key' >&2; exit 2; }
if [ -n "${BENCH_CACHE_DIRECTORY:-}" ]; then
    cache="$BENCH_CACHE_DIRECTORY/$kind-$arch-$key"
    mkdir -p "$directory"
    if [ "$operation" = pull ]; then
        [ -f "$cache/complete" ] || exit 3
        cp -R "$cache/files/." "$directory/"
    else
        mkdir -p "$BENCH_CACHE_DIRECTORY"
        temporary="$(mktemp -d "$BENCH_CACHE_DIRECTORY/.pending.XXXXXX")"
        trap 'rm -rf "$temporary"' EXIT
        mkdir "$temporary/files"
        for name in "$@"; do
            [[ "$name" != */* && "$name" != .* ]] || { echo 'invalid cache filename' >&2; exit 2; }
            cp "$directory/$name" "$temporary/files/"
        done
        touch "$temporary/complete"
        if [ ! -d "$cache" ]; then mv "$temporary" "$cache"; fi
    fi
    printf 'Local %s cache %s: %s\n' "$kind" "$operation" "$cache"
    exit
fi
if [ -z "$registry" ]; then
    [ "$operation" != pull ] || exit 3
    exit 0
fi
command -v oras >/dev/null || { echo 'GHCR reuse needs the pinned ORAS CLI' >&2; exit 1; }
mkdir -p "$directory"
config="$(mktemp -d)"
trap 'rm -rf "$config"' EXIT
auth=(--registry-config "$config/config.json")
if [ -n "${GH_TOKEN:-}" ]; then
    printf '%s' "$GH_TOKEN" | oras login "${registry%%/*}" \
        --username "${GITHUB_ACTOR:?registry actor required}" --password-stdin "${auth[@]}" >/dev/null
fi
reference="$registry:$kind-$arch-$key"
if [ "$operation" = pull ]; then
    if digest="$(oras resolve "$reference" "${auth[@]}" 2>"$config/resolve.log")"; then
        cat "$config/resolve.log" >&2
    else
        cat "$config/resolve.log" >&2
        if grep -Eiq 'manifest[ _]unknown|name[ _]unknown' "$config/resolve.log" ||
            grep -Fq "$reference: not found" "$config/resolve.log"; then
            printf 'OCI cache miss: %s\n' "$reference"
            exit 3
        fi
        exit 1
    fi
    [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || { echo 'invalid OCI cache manifest digest' >&2; exit 1; }
    immutable="$registry@$digest"
    if oras pull "$immutable" --output "$directory" "${auth[@]}" >"$config/pull.log" 2>&1; then
        cat "$config/pull.log"
        printf '%s\n' "$immutable" >"$directory/cache-reference.txt"
    else
        cat "$config/pull.log" >&2
        exit 1
    fi
else
    cd "$directory"
    oras push "$reference" "${auth[@]}" \
        --artifact-type "application/vnd.antimatter.ext4.qemu-$kind.v1" \
        --annotation "org.opencontainers.image.source=https://github.com/${GITHUB_REPOSITORY:?repository required}" "$@"
fi
