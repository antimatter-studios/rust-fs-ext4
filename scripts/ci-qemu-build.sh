#!/usr/bin/env bash
set -euo pipefail

if [ "${1:-}" = container ]; then
    unset GH_TOKEN GITHUB_TOKEN
    arch="${2:?target architecture required}"
    workload="${3:?workload required}"
    evidence="${BUILD_EVIDENCE:-/evidence}"
    mkdir -p "$evidence"
    test "$(uname -m)" = "$BUILD_HOST_ARCH"
    cd /repo
    export DEBIAN_FRONTEND=noninteractive
    export RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo CARGO_TARGET_DIR=/benchmark-target
    export PATH="$CARGO_HOME/bin:$PATH" FS_CORE_CALLER=/repo
    test -f /opt/ext4-runner.json
    expected="$(python3 scripts/ci-qemu-artifacts.py runner-recipe /repo "$BUILD_HOST_ARCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["key"])')"
    python3 - "$expected" "$BUILD_HOST_ARCH" <<'PY'
import json, sys
assert json.load(open('/opt/ext4-runner.json')) == {'key': sys.argv[1], 'arch': sys.argv[2]}, 'prepared runner mismatch'
PY
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc
    rustc -vV >"$evidence/rustc.txt"
    uname -m >"$evidence/build-host-arch.txt"
    dpkg-query -W >"$evidence/build-packages.txt"
    targets=()
    while IFS= read -r argument; do
        [ -z "$argument" ] || targets+=("$argument")
    done < <(python3 scripts/ci-qemu-artifacts.py targets "$workload")
    printf '[%s] native compiler=%s target=%s workload=%s\n' "$(date -u +%FT%TZ)" "$BUILD_HOST_ARCH" "$arch" "$workload"
    /usr/bin/time -v -o "$evidence/compile.time" cargo test --locked --features cli \
        --target "$arch-unknown-linux-gnu" "${targets[@]+"${targets[@]}"}" --no-run --message-format=json \
        | tee "$evidence/artifacts.jsonl" | python3 -u -c 'import json,sys
for line in sys.stdin:
    record = json.loads(line)
    if record.get("reason") == "compiler-message": print(record["message"].get("rendered", ""), end="", flush=True)
    elif record.get("reason") == "compiler-artifact": print("built:", record["target"]["name"], flush=True)'
    python3 scripts/ci-qemu-artifacts.py pack "$evidence" "$arch" "$workload"
    exit
fi

arch="${1:?target architecture required}"
workload="${2:?workload required}"
state="${3:?evidence directory required}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
if [ "${QEMU_RUNNER_ENVIRONMENT:-}" = 1 ]; then
    printf '%s\n' "${BENCH_RUNNER_IMAGE:?prepared runner digest required}" >"$state/builder-image.txt"
    BUILD_HOST_ARCH="$(uname -m)" BUILD_EVIDENCE="$state" bash "$repo/scripts/ci-qemu-build.sh" container "$arch" "$workload"
    exit
fi
command -v docker >/dev/null || { echo 'host compilation needs Docker or a Docker-compatible daemon' >&2; exit 1; }
host_arch="$(docker info --format '{{.Architecture}}')"
case "$host_arch" in amd64|x86_64) host_arch=x86_64; platform=amd64 ;; arm64|aarch64) host_arch=aarch64; platform=arm64 ;; *) echo "unsupported native build host: $host_arch" >&2; exit 1 ;; esac
base="$(python3 "$repo/scripts/ci-qemu-artifacts.py" builder)"
case "$base" in debian:12-slim@sha256:*) ;; *) echo 'compiler image must pin the Debian 12 digest' >&2; exit 1 ;; esac
builder="${BENCH_RUNNER_IMAGE:-$(bash "$repo/scripts/ci-qemu-environment.sh" resolve "$host_arch")}"
[[ "$builder" =~ (^sha256:|@sha256:)[0-9a-f]{64}$ ]] || { echo 'prepared builder must use an immutable image digest' >&2; exit 1; }
printf '%s\n' "$builder" >"$state/builder-image.txt"
# No CPU or memory limit on the build container: the platform sets what the
# runner may use. Cargo sizes its jobs from the CPUs it can see.
docker run --rm --platform "linux/$platform" \
    -e "BUILD_HOST_ARCH=$host_arch" \
    -v "$repo:/repo:ro" -v "$repo/../rust-fs-core:/rust-fs-core:ro" -v "$state:/evidence" \
    "$builder" bash /repo/scripts/ci-qemu-build.sh container "$arch" "$workload"
