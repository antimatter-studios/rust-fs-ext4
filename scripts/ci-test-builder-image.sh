#!/usr/bin/env bash
set -euo pipefail

registry="ghcr.io/${GITHUB_REPOSITORY,,}/test-builder"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/recipe"
bash scripts/test-builder-context.sh "$scratch/recipe"
recipe="$(cd "$scratch/recipe" && sha256sum Dockerfile rust-toolchain.toml core-toolchain.sh | sha256sum | cut -d' ' -f1)"
tag="$registry:amd64-$recipe"

export DOCKER_HOST="${DOCKER_HOST:-$(docker context inspect --format '{{.Endpoints.docker.Host}}')}"
export DOCKER_CONFIG="$scratch/docker-config"
mkdir -p "$DOCKER_CONFIG"
printf '%s' "${GH_TOKEN:?GHCR token required}" | docker login ghcr.io --username "$GITHUB_ACTOR" --password-stdin >&2
if ! docker manifest inspect --verbose "$tag" >"$scratch/manifest.json" 2>"$scratch/manifest.log"; then
    cat "$scratch/manifest.log" >&2
    grep -Eiq 'manifest unknown|manifest_unknown|no such manifest' "$scratch/manifest.log" || exit 1
    if [ "${PUBLISH_BUILDER:-false}" != true ]; then
        echo "No builder image for this recipe; warm $tag from a trusted branch." >&2
        exit 1
    fi
    docker build --platform linux/amd64 --build-arg "RECIPE_KEY=$recipe" \
        --build-arg "SOURCE_REPOSITORY=https://github.com/$GITHUB_REPOSITORY" --tag "$tag" "$scratch/recipe" >&2
    docker push "$tag" >&2
    docker manifest inspect --verbose "$tag" >"$scratch/manifest.json"
fi
digest="$(python3 -c '
import json, re, sys
manifest = json.load(sys.stdin)
digest = manifest.get("Descriptor", {}).get("digest", "") if isinstance(manifest, dict) else ""
if not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
    sys.exit("Expected a single registry image manifest with a SHA-256 digest")
print(digest)
' <"$scratch/manifest.json")"
image="$registry@$digest"
docker pull --platform linux/amd64 "$image" >&2
actual="$(docker image inspect --format '{{index .Config.Labels "io.antimatter.ext4.builder-key"}}/{{.Architecture}}' "$image")"
[ "$actual" = "$recipe/amd64" ] || { echo 'Builder image recipe or architecture mismatch' >&2; exit 1; }
printf '%s\n' "$image"
