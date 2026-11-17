#!/usr/bin/env bash
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
operation="${1:?run or prepare required}"
arch="${2:?guest architecture required}"
accelerator="${3:?accelerator required}"
workload="${4:-single}"
case "$operation/$accelerator" in run/tcg|run/kvm|prepare/tcg|prepare/kvm) ;; *) echo 'unsupported pipeline runner operation' >&2; exit 2 ;; esac
mkdir -p "$repo/tmp/qemu-benchmark"
python3 "$repo/scripts/ci-qemu-artifacts.py" runner-recipe "$repo" "$(docker info --format '{{.Architecture}}')" \
    >"$repo/tmp/qemu-benchmark/runner-environment.json"
pull_started="$(date +%s)"
image="$(bash "$repo/scripts/ci-qemu-environment.sh" resolve)"
printf '%s\n' "$(( $(date +%s) - pull_started ))" >"$repo/tmp/qemu-benchmark/runner-image-pull-seconds.txt"
[[ "$image" =~ @sha256:[0-9a-f]{64}$ ]] || { echo 'runner must use a GHCR digest' >&2; exit 1; }
printf '%s\n' "$image" >"$repo/tmp/qemu-benchmark/runner-image.txt"
device=()
if [ "$accelerator" = kvm ]; then
    test -r /dev/kvm && test -w /dev/kvm || { echo 'requested KVM device unavailable' >&2; exit 1; }
    device=(--device /dev/kvm)
fi
environment=()
for name in GH_TOKEN GITHUB_ACTOR GITHUB_REPOSITORY GITHUB_RUN_ID GITHUB_RUN_ATTEMPT GITHUB_JOB GITHUB_SHA \
    GITHUB_ACTIONS BENCH_CACHE_REGISTRY BENCH_READY_AT BENCH_JOB_STARTED_AT BENCH_REQUESTED_RUNNER \
    BENCH_SELECTED_RUNNER BENCH_RUNNER_ENVIRONMENT BENCH_RUNNER_NAME QEMU_TCG_CPU QEMU_TCG_ACCEL; do
    if [ -n "${!name:-}" ]; then environment+=(-e "$name"); fi
done
# A GitHub-hosted runner is a whole VM that exists for this job, so the job
# may use all of it. Every other runner (the antimatter-vps pods) shares its
# node with other jobs and stays capped. BENCH_CPUS / BENCH_MEMORY override.
if [ "${BENCH_RUNNER_ENVIRONMENT:-}" = github-hosted ]; then
    cpus="${BENCH_CPUS:-$(nproc)}"
    memory_args=()
    [ -z "${BENCH_MEMORY:-}" ] || memory_args=(--memory="$BENCH_MEMORY")
else
    cpus="${BENCH_CPUS:-2}"
    memory_args=(--memory="${BENCH_MEMORY:-4g}")
fi
docker run --rm --cpus="$cpus" "${memory_args[@]+"${memory_args[@]}"}" "${device[@]+"${device[@]}"}" \
    "${environment[@]+"${environment[@]}"}" -e "BENCH_RUNNER_IMAGE=$image" -e "BENCH_CPUS=$cpus" \
    -e "EVIDENCE_OWNER=$(id -u):$(id -g)" \
    -v "$repo:/repo" -v "$repo/../rust-fs-core:/rust-fs-core:ro" \
    -w /repo "$image" bash -c '
        set -euo pipefail
        trap '\''chown -R "$EVIDENCE_OWNER" /repo/tmp/qemu-benchmark /repo/tmp/qemu-images 2>/dev/null || true'\'' EXIT
        git config --global --add safe.directory /repo
        git config --global --add safe.directory /rust-fs-core
        bash scripts/ci-qemu-benchmark.sh "$@"
    ' -- "$operation" "$arch" "$accelerator" "$workload"
