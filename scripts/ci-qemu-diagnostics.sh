#!/usr/bin/env bash
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$repo"
for directory in tmp/qemu-benchmark tmp/qemu-images; do
    if [ -d "$directory" ] && [ ! -L "$directory" ]; then
        sudo -n chown -hR "$(id -u):$(id -g)" -- "$directory"
    fi
done
