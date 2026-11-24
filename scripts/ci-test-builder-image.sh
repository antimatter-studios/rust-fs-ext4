#!/usr/bin/env bash
set -euo pipefail

registry="ghcr.io/${GITHUB_REPOSITORY,,}/test-builder"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/recipe"
cp scripts/ci-test-builder/Dockerfile rust-toolchain.toml "$scratch/recipe/"
cp ../rust-fs-core/scripts/guest-rust-toolchain.sh "$scratch/recipe/core-toolchain.sh"
recipe="$(cd "$scratch/recipe" && sha256sum Dockerfile rust-toolchain.toml core-toolchain.sh | sha256sum | cut -d' ' -f1)"
tag="$registry:amd64-$recipe"

export DOCKER_HOST="${DOCKER_HOST:-$(docker context inspect --format '{{.Endpoints.docker.Host}}')}"
export DOCKER_CONFIG="$scratch/docker-config"
mkdir -p "$DOCKER_CONFIG"
printf '%s' "${GH_TOKEN:?GHCR token required}" | docker login ghcr.io --username "$GITHUB_ACTOR" --password-stdin >&2
if ! docker pull --platform linux/amd64 "$tag" >"$scratch/pull.log" 2>&1; then
    cat "$scratch/pull.log" >&2
    grep -Eiq 'manifest unknown|manifest_unknown|not found' "$scratch/pull.log" || exit 1
    if [ "${PUBLISH_BUILDER:-false}" != true ]; then
        echo "No builder image for this recipe; warm $tag from a trusted branch." >&2
        exit 1
    fi
    docker build --platform linux/amd64 --build-arg "RECIPE_KEY=$recipe" \
        --build-arg "SOURCE_REPOSITORY=https://github.com/$GITHUB_REPOSITORY" --tag "$tag" "$scratch/recipe" >&2
    docker push "$tag" >&2
fi
actual="$(docker image inspect --format '{{index .Config.Labels "io.antimatter.ext4.builder-key"}}/{{.Architecture}}' "$tag")"
[ "$actual" = "$recipe/amd64" ] || { echo 'Builder image recipe or architecture mismatch' >&2; exit 1; }
docker image inspect --format '{{json .RepoDigests}}' "$tag" |
    python3 -c 'import json, sys; print(next(item for item in json.load(sys.stdin) if item.startswith(sys.argv[1] + "@sha256:")))' "$registry"
