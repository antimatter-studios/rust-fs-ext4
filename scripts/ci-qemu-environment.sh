#!/usr/bin/env bash
set -euo pipefail
operation="${1:?usage: ci-qemu-environment.sh reference|build|publish|resolve|prepare|local [architecture]}"
case "$operation" in reference|build|publish|resolve|prepare|local) ;; *) echo 'unknown environment operation' >&2; exit 2 ;; esac
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
arch="${2:-$(docker info --format '{{.Architecture}}')}"
case "$arch" in
    amd64|x86_64) arch=x86_64; platform=amd64 ;;
    arm64|aarch64) arch=aarch64; platform=arm64 ;;
    *) echo "unsupported runner architecture: $arch" >&2; exit 1 ;;
esac
registry="${BENCH_CACHE_REGISTRY:-ghcr.io/antimatter-studios/rust-fs-ext4/qemu-pilot}"
[[ "$registry" == ghcr.io/* ]] || { echo 'runner images must use GHCR' >&2; exit 1; }
recipe="$(python3 "$repo/scripts/ci-qemu-artifacts.py" runner-recipe "$repo" "$arch")"
key="$(printf '%s' "$recipe" | python3 -c 'import json,sys; print(json.load(sys.stdin)["key"])')"
reference="$registry:runner-$arch-$key"
if [ "$operation" = reference ]; then printf '%s\n' "$reference"; exit; fi
if [ "$operation" = prepare ]; then
    if bash "$repo/scripts/ci-qemu-environment.sh" resolve "$arch"; then
        exit
    else
        cache_status=$?
        [ "$cache_status" = 3 ] || exit "$cache_status"
    fi
    bash "$repo/scripts/ci-qemu-environment.sh" build "$arch" >&2
    exec bash "$repo/scripts/ci-qemu-environment.sh" publish "$arch"
fi
temporary="$(mktemp -d)"
trap 'rm -rf "$temporary"' EXIT
if [ "$operation" = build ]; then
    cp "$repo/scripts/qemu-runner/"{Dockerfile,packages.txt,setup.sh} "$temporary/"
    cp "$repo/rust-toolchain.toml" "$temporary/"
    cp "$repo/scripts/qemu-pilot-environment.json" "$temporary/environment.json"
    cp "$repo/../rust-fs-core/scripts/guest-rust-toolchain.sh" "$temporary/core-toolchain.sh"
    base="$(python3 "$repo/scripts/ci-qemu-artifacts.py" builder)"
    case "$base" in debian:12-slim@sha256:*) ;; *) echo 'runner base must pin Debian 12' >&2; exit 1 ;; esac
    docker build --platform "linux/$platform" --progress=plain \
        --build-arg "BASE_IMAGE=$base" --build-arg "ENVIRONMENT_ID=$key" \
        --build-arg "SOURCE_REPOSITORY=https://github.com/${GITHUB_REPOSITORY:-antimatter-studios/rust-fs-ext4}" \
        --tag "$reference" "$temporary"
    printf 'Prepared runner image: %s\n' "$reference"
    exit
fi
if [ "$operation" = resolve ] || [ "$operation" = publish ]; then
    if [ -n "${GH_TOKEN:-}" ]; then
        endpoint="${DOCKER_HOST:-$(docker context inspect --format '{{.Endpoints.docker.Host}}')}"
        export DOCKER_HOST="$endpoint" DOCKER_CONFIG="$temporary/docker"
        mkdir -p "$DOCKER_CONFIG"
        printf '%s' "$GH_TOKEN" | docker login ghcr.io \
            --username "${GITHUB_ACTOR:?registry actor required}" --password-stdin >&2
    fi
fi
if [ "$operation" = resolve ]; then
    if ! docker pull --platform "linux/$platform" "$reference" >"$temporary/pull.log" 2>&1; then
        cat "$temporary/pull.log" >&2
        echo "Prepared runner unavailable: $reference; build and publish it locally first" >&2
        if grep -Eiq 'manifest unknown|manifest_unknown|not found' "$temporary/pull.log"; then exit 3; fi
        exit 1
    fi
    cat "$temporary/pull.log" >&2
fi
if [ "$operation" = publish ]; then
    if docker pull --platform "linux/$platform" "$reference" >"$temporary/existing.log" 2>&1; then
        operation=resolve
        printf 'Reusing published recipe without overwriting it: %s\n' "$reference" >&2
    elif ! grep -Eiq 'manifest unknown|manifest_unknown|not found' "$temporary/existing.log"; then
        cat "$temporary/existing.log" >&2
        exit 1
    fi
fi
label="$(docker image inspect --format '{{index .Config.Labels "io.antimatter.ext4.runner-key"}}' "$reference")"
actual="$(docker image inspect --format '{{.Architecture}}' "$reference")"
[ "$label/$actual" = "$key/$platform" ] || { echo 'runner image recipe or architecture mismatch' >&2; exit 1; }
if [ "$operation" = publish ]; then docker push "$reference" >&2; fi
if [ "$operation" = local ]; then
    docker image inspect --format '{{.Id}}' "$reference"
else
    docker image inspect --format '{{json .RepoDigests}}' "$reference" | python3 -c '
import json, sys
digests = [item for item in json.load(sys.stdin) if item.startswith(sys.argv[1] + "@sha256:")]
if len(digests) != 1:
    sys.exit("prepared runner has no unique GHCR digest")
print(digests[0])' "$registry"
fi
