#!/usr/bin/env bash
set -euo pipefail

workspace="$(pwd -P)"
base=/opt/ext4-ci
mkdir "$base/rust-fs-ext4" "$base/rust-fs-core" "$base/fs-linux-test-harness"
rsync -aS --exclude=/target --exclude=/tmp "$workspace/" "$base/rust-fs-ext4/"
for sibling in rust-fs-core fs-linux-test-harness; do
    rsync -aS --exclude=/target --exclude=/tmp "$workspace/../$sibling/" "$base/$sibling/"
done
