#!/usr/bin/env bash
set -euo pipefail

arch="${1:?guest architecture required}"
image="${2:?builder image required}"
key="${3:?bundle key required}"
case "$arch" in x86_64|aarch64) ;; *) exit 2 ;; esac
[[ "$image" =~ ^ghcr\.io/[^@]+@sha256:[0-9a-f]{64}$ && "$key" =~ ^[0-9a-f]{64}$ ]] || {
    echo 'Expected an immutable builder image and a SHA-256 bundle key' >&2
    exit 2
}

repo="$(cd "${GITHUB_WORKSPACE:?}" && pwd -P)"
core="$(cd "$repo/../rust-fs-core" && pwd -P)"
harness="$(cd "$repo/../fs-linux-test-harness" && pwd -P)"
output="$repo/tmp/ci-test-bundle"
mkdir -p "$output" "$repo/tmp/ci-bin"
install -m 0755 "$(command -v chore)" "$repo/tmp/ci-bin/chore"

export DOCKER_HOST="${DOCKER_HOST:-$(docker context inspect --format '{{.Endpoints.docker.Host}}')}"
export DOCKER_CONFIG
DOCKER_CONFIG="$(mktemp -d)"
trap 'rm -rf "$DOCKER_CONFIG"' EXIT
printf '%s' "${GH_TOKEN:?GHCR token required}" | docker login ghcr.io --username "${GITHUB_ACTOR:?}" --password-stdin
docker pull --platform linux/amd64 "$image"
docker run --rm --pull=never --platform linux/amd64 \
    --mount "type=bind,src=$repo,dst=/source/rust-fs-ext4,readonly" \
    --mount "type=bind,src=$core,dst=/source/rust-fs-core,readonly" \
    --mount "type=bind,src=$harness,dst=/source/fs-linux-test-harness,readonly" \
    --mount "type=bind,src=$output,dst=/bundle" \
    --workdir /source/rust-fs-ext4 "$image" bash -c '
        set -euo pipefail
        arch="$1"
        key="$2"
        install -m 0755 tmp/ci-bin/chore /usr/local/bin/chore
        mkdir /opt/ext4-ci
        bash scripts/ci-test-workspace.sh
        cd /opt/ext4-ci/rust-fs-ext4
        git config --global --add safe.directory "$PWD"
        export CARGO_BUILD_TARGET="$arch-unknown-linux-gnu" CARGO_BUILD_JOBS=1
        if [ "$arch" = aarch64 ]; then
            export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
            export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
            export AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
        fi
        chore lint
        python3 scripts/ci-test-bundle.py build "$arch" "$key" /bundle/bundle.tar
    ' -- "$arch" "$key"
