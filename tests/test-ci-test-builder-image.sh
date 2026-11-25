#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
export GITHUB_REPOSITORY=Example/ext4 GITHUB_ACTOR=runner GH_TOKEN=test-token
export PUBLISH_BUILDER=true DOCKER_HOST=unix:///test-docker.sock
export TEST_REGISTRY=ghcr.io/example/ext4/test-builder
export TEST_DIGEST=sha256:9e123d8aa20d664de07d8842e7fb91319211a279b1436d1a2c21d659275db8b0
export TEST_STATE="$scratch/state" TEST_TRACE="$scratch/trace"

cat >"$scratch/docker" <<'DOCKER'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$TEST_TRACE"
case "$1 $2" in
    'login ghcr.io') [[ "$(cat)" == test-token ]] ;;
    'manifest inspect')
        [[ "$3" == --verbose && "$4" == "$TEST_REGISTRY:amd64-"* ]]
        case "$(cat "$TEST_STATE")" in
            missing) echo 'manifest unknown' >&2; exit 1 ;;
            denied) echo 'denied: permission denied' >&2; exit 1 ;;
        esac
        printf '{"Descriptor":{"digest":"%s"}}\n' "$TEST_DIGEST"
        ;;
    'build --platform') ;;
    "push $TEST_REGISTRY:amd64-"*) echo present > "$TEST_STATE" ;;
    'pull --platform')
        case "$4:$(cat "$TEST_STATE")" in
            "$TEST_REGISTRY:amd64-"*:present|"$TEST_REGISTRY@$TEST_DIGEST:present") ;;
            *) echo 'manifest unknown' >&2; exit 1 ;;
        esac
        ;;
    'image inspect')
        if [[ "$4" == *RepoDigests* ]]; then
            printf '["%s@sha256:f4d9336c2d34b1607c219d83bd0414bcd81cdaba8cafea30d818f15bd67049a2"]\n' "$TEST_REGISTRY"
        else
            if [[ "$5" == "$TEST_REGISTRY@$TEST_DIGEST" ]]; then
                tag="$(sed -n 's/^manifest inspect --verbose //p' "$TEST_TRACE" | tail -1)"
            else
                tag="$5"
            fi
            printf '%s/amd64\n' "${tag##*:amd64-}"
        fi
        ;;
    *) echo "unexpected docker invocation: $*" >&2; exit 1 ;;
esac
DOCKER
chmod +x "$scratch/docker"
export PATH="$scratch:$PATH"

resolve() {
    printf '%s\n' "$1" > "$TEST_STATE"
    : > "$TEST_TRACE"
    bash scripts/ci-test-builder-image.sh 2> "$scratch/stderr"
}

for state in missing present; do
    if ! image="$(resolve "$state")" || [[ "$image" != "$TEST_REGISTRY@$TEST_DIGEST" ]]; then
        echo "$state: did not resolve the registry digest" >&2
        cat "$scratch/stderr" >&2
        exit 1
    fi
    grep -Fxq "pull --platform linux/amd64 $image" "$TEST_TRACE"
    if [[ "$state" == missing ]]; then
        grep -q '^push ' "$TEST_TRACE"
    else
        ! grep -Eq '^(build|push) ' "$TEST_TRACE" || exit 1
    fi
done

for state in denied unpullable; do
    if image="$(resolve "$state")" || [[ -n "$image" ]]; then
        echo "$state: published an unusable image reference" >&2
        exit 1
    fi
    ! grep -Eq '^(build|push) ' "$TEST_TRACE" || exit 1
done
echo 'builder image resolves and verifies the registry digest on both cache paths'
