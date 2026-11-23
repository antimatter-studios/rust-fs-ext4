#!/usr/bin/env bash
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$repo"
image_file=tmp/qemu-benchmark/runner-image.txt
image=''
for directory in tmp/qemu-benchmark tmp/qemu-images; do
    if [ -d "$directory" ] && [ ! -L "$directory" ]; then
        if [ -z "$image" ]; then
            if [ ! -f "$image_file" ] || [ -L "$image_file" ]; then
                echo 'diagnostic recovery requires the recorded immutable runner image' >&2
                exit 1
            fi
            image="$(cat "$image_file")"
            if [[ ! "$image" =~ ^ghcr\.io/[a-z0-9._/-]+@sha256:[0-9a-f]{64}$ ]]; then
                echo 'diagnostic recovery requires a GHCR runner digest, not a mutable tag' >&2
                exit 1
            fi
        fi
        docker run --rm --pull=never --network none --read-only \
            --cap-drop ALL --cap-add CHOWN --cap-add DAC_OVERRIDE \
            --security-opt no-new-privileges --user 0:0 \
            --mount "type=bind,src=$repo/$directory,dst=/evidence" \
            --entrypoint chown "$image" -hRP "$(id -u):$(id -g)" -- /evidence
    fi
done
