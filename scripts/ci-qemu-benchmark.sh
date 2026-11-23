#!/usr/bin/env bash
set -euo pipefail

if [ "${QEMU_BENCHMARK_FROZEN:-}" != 1 ]; then
    export QEMU_BENCHMARK_FROZEN=1
    QEMU_BENCHMARK_REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
    export QEMU_BENCHMARK_REPO
    exec bash -c "$(cat "${BASH_SOURCE[0]}")" -- "$@"
fi

action="${1:-}"
arch="${2:-}"
accel="${3:-}"
workload="${4:-smoke}"
case "$action" in plan|run|prepare|prepare-local|serve) ;; *) echo 'usage: ci-qemu-benchmark.sh plan|run|prepare|prepare-local x86_64|aarch64 kvm|tcg|hvf [single|smoke|full]
       ci-qemu-benchmark.sh serve x86_64|aarch64 kvm|tcg full -- <command...>' >&2; exit 2 ;; esac
case "$arch" in amd64) arch=x86_64 ;; arm64) arch=aarch64 ;; esac
case "$arch/$accel" in x86_64/kvm|x86_64/tcg|aarch64/tcg|aarch64/hvf) ;; *) echo "unsupported guest/accelerator: $arch/$accel" >&2; exit 2 ;; esac
if [ "$action" = prepare-local ] && [ "${GITHUB_ACTIONS:-}" = true ]; then
    echo 'prepare-local is forbidden in GitHub Actions' >&2
    exit 1
fi
preparation=false
case "$action" in prepare|prepare-local) preparation=true ;; esac
case "$workload" in single|smoke|full) ;; *) echo "unsupported workload: $workload" >&2; exit 2 ;; esac
# SERVE: boot the guest the `full` preparation describes, then run a command
# HERE, beside it, with FS_EXT4_GUEST_SSH naming an ssh config whose host
# `guest` is that guest. CI's VM jobs run their chore tasks this way on a
# runner without KVM: they build on this host and run in the guest. A guest
# cache miss sets the same tools up in this live guest and publishes nothing.
serve_command=()
if [ "$action" = serve ]; then
    [ "$workload" = full ] || { echo 'serve uses the full guest preparation' >&2; exit 2; }
    shift 4 || { echo 'serve needs -- <command...>' >&2; exit 2; }
    [ "${1:-}" = -- ] && shift
    [ "$#" -gt 0 ] || { echo 'serve needs -- <command...>' >&2; exit 2; }
    serve_command=("$@")
fi

disk=guest.qcow2
seed=seed.iso
console=console.log
port=2222
firmware="${QEMU_ARM_FIRMWARE:-/usr/share/AAVMF/AAVMF_CODE.fd}"
qemu_args() {
    local machine=q35 cpu=host accelerator=kvm
    # QEMU_TCG_CPU / QEMU_TCG_ACCEL select the emulated CPU and TCG options
    # for experiments (see qemu-tcg-flags.yml); the defaults are what every
    # other run uses.
    if [ "$accel" = tcg ]; then cpu="${QEMU_TCG_CPU:-max}"; accelerator="${QEMU_TCG_ACCEL:-tcg,thread=multi}"; fi
    if [ "$accel" = hvf ]; then accelerator=hvf; fi
    [ "$arch" != aarch64 ] || machine=virt
    # What the timing line and result.json report as this guest's setup.
    qemu_summary="machine=$machine cpu=$cpu accel=$accelerator memory_mib=2048 smp=2"
    command_args=("qemu-system-$arch" -machine "$machine" -accel "$accelerator"
        -cpu "$cpu" -m 2048 -smp 2 -display none -monitor none -no-reboot
        -serial "file:$console" -drive "file=$disk,if=virtio,format=qcow2,discard=unmap"
        -drive "file=$seed,if=virtio,format=raw,readonly=on"
        -netdev "user,id=net0,net=10.0.2.0/24,ipv6=off,hostfwd=tcp:127.0.0.1:$port-:22"
        -device 'virtio-net-pci,netdev=net0,mac=52:54:00:12:34:56')
    [ "$arch" != aarch64 ] || command_args+=(-bios "$firmware")
}

if [ "$action" = plan ]; then
    qemu_args
    printf '%q ' "${command_args[@]}"
    printf '\n'
    exit 0
fi

repo="${QEMU_BENCHMARK_REPO:?benchmark repository required}"
cd "$repo"
if "$preparation" && [ -z "${BENCH_CACHE_REGISTRY:-}${BENCH_CACHE_DIRECTORY:-}" ]; then
    echo 'guest preparation requires BENCH_CACHE_REGISTRY or BENCH_CACHE_DIRECTORY so its image is reusable' >&2
    exit 1
fi
for tool in qemu-img "qemu-system-$arch" python3 curl ssh ssh-keygen tar; do
    command -v "$tool" >/dev/null || { echo "missing $tool; install the QEMU benchmark host tools" >&2; exit 1; }
done
if [ "$accel" = kvm ]; then
    [ "$(uname -s)/$(uname -m)" = Linux/x86_64 ] && [ -r /dev/kvm ] && [ -w /dev/kvm ] || {
        echo 'KVM was requested but /dev/kvm is not usable on this x86_64 Linux host; no silent TCG fallback' >&2
        exit 1
    }
fi
if [ "$accel" = hvf ] && [ "$(uname -s)/$(uname -m)" != Darwin/arm64 ]; then
    echo 'HVF requires a native Apple Silicon host; no silent TCG fallback' >&2
    exit 1
fi
if [ "$arch" = aarch64 ]; then
    [ -r "$firmware" ] || { echo "missing ARM64 UEFI firmware: $firmware" >&2; exit 1; }
fi
[ -d ../rust-fs-core ] || { echo "run 'chore siblings' first" >&2; exit 1; }

mkdir -p tmp/qemu-benchmark tmp/qemu-images
state="$(mktemp -d "$repo/tmp/qemu-benchmark/$arch-$accel.XXXXXX")"
# CI's VM jobs serve as root in a container over the host runner's workspace
# (ci-qemu-runner.sh serve). The runner owns the evidence directory from the
# start, still mode 0700, so it can upload the evidence even when the
# container dies before its EXIT trap hands the files back. Files kept
# private (the guest's SSH key) stay root's and unreadable to the runner.
if [ "$action" = serve ] && [ -n "${EVIDENCE_OWNER:-}" ]; then
    chown "$EVIDENCE_OWNER" "$state"
fi
disk="$state/guest.qcow2"
seed="$state/seed.iso"
console="$state/console.log"
qemu_pid=''
sampler_pid=''
started="$(date +%s)"
progress() {
    printf '%s\n' "$*" >"$state/phase"
    printf '[%s] %s/%s %s; evidence=%s\n' "$(date -u +%FT%TZ)" "$arch" "$accel" "$*" "$state"
}
cleanup() {
    status=$?
    trap - EXIT
    failed_phase="$(cat "$state/phase" 2>/dev/null || true)"
    printf '[%s] stopping; status=%s; phase=%s\n' "$(date -u +%FT%TZ)" "$status" "$failed_phase"
    if [ -n "$sampler_pid" ]; then
        kill "$sampler_pid" 2>/dev/null || true
        wait "$sampler_pid" 2>/dev/null || true
    fi
    if [ -n "$qemu_pid" ]; then
        kill "$qemu_pid" 2>/dev/null || true
        for _ in $(seq 1 30); do
            kill -0 "$qemu_pid" 2>/dev/null || break
            sleep 1
        done
        if kill -0 "$qemu_pid" 2>/dev/null; then kill -KILL "$qemu_pid" 2>/dev/null || true; fi
        wait "$qemu_pid" 2>/dev/null || true
    fi
    # One line per guest step, whatever the outcome: where it ran, how the
    # guest was emulated, how long it took to boot and how long the work
    # beside it or in it took. Unknown is said, not guessed.
    boot_seconds="$(cat "$state/boot-seconds.txt" 2>/dev/null || echo unknown)"
    workload_seconds="$(cat "$state/workload-seconds.txt" 2>/dev/null || echo unknown)"
    printf '[%s] timing runner=%s arch=%s accelerator=%s qemu=[%s] host_cpus=%s build_jobs=%s test_jobs=%s boot_seconds=%s workload_seconds=%s elapsed_seconds=%s status=%s\n' \
        "$(date -u +%FT%TZ)" "${RUNNER_NAME:-${BENCH_RUNNER_NAME:-local}}" "$arch" "$accel" "${qemu_summary:-unknown}" \
        "$(nproc 2>/dev/null || echo unknown)" "${CARGO_BUILD_JOBS:-unset}" "${RUST_TEST_THREADS:-unset}" \
        "$boot_seconds" "$workload_seconds" "$(( $(date +%s) - started ))" "$status" | tee "$state/timing.txt"
    python3 - "$state/result.json" "$arch" "$accel" "$workload" "$started" "$status" "$action" "$failed_phase" \
        "$boot_seconds" "$workload_seconds" "${qemu_summary:-unknown}" <<'PY'
import json, os, pathlib, sys, time
path, arch, accel, workload, started, status, purpose, phase, boot, work, qemu = sys.argv[1:]
finished = int(time.time())
def seconds(value):
    return int(value) if value.isdigit() else None
samples = pathlib.Path(path).with_name("qemu-rss-kib.txt")
rss = [int(line.strip()) for line in samples.read_text().splitlines() if line.strip()] if samples.exists() else []
pathlib.Path(path).write_text(json.dumps({
    "guest_arch": arch, "accelerator": accel, "workload": workload,
    "status": int(status), "phase": phase, "elapsed_seconds": finished - int(started),
    "started_unix": int(started), "finished_unix": finished,
    "job_ready_unix": int(os.getenv("BENCH_READY_AT", started)),
    "job_arrival_unix": int(os.getenv("BENCH_JOB_STARTED_AT", started)),
    "guest_memory_mib": 2048, "guest_cpus": 2,
    "tcg_cpu": os.getenv("QEMU_TCG_CPU", "max"),
    "tcg_accel": os.getenv("QEMU_TCG_ACCEL", "tcg,thread=multi"),
    "build_mode": "host-prebuilt",
    "purpose": purpose,
    "qemu_peak_rss_kib": max(rss, default=0),
    "runner_environment": os.getenv("BENCH_RUNNER_ENVIRONMENT", "local"),
    "requested_runner": os.getenv("BENCH_REQUESTED_RUNNER", "local"),
    "selected_runner": os.getenv("BENCH_SELECTED_RUNNER", "local"),
    "run_id": os.getenv("GITHUB_RUN_ID"), "commit": os.getenv("GITHUB_SHA"),
    "runner_name": os.getenv("RUNNER_NAME") or os.getenv("BENCH_RUNNER_NAME") or "local",
    "qemu": qemu, "boot_seconds": seconds(boot), "workload_seconds": seconds(work),
    "build_jobs": os.getenv("CARGO_BUILD_JOBS"), "test_jobs": os.getenv("RUST_TEST_THREADS"),
}, indent=2) + "\n")
PY
    rm -f "$disk" "$seed" "$state/key" "$state/key.pub" "$state/payload.tar.gz"
    rm -f "$state/core-payload.tar.gz"
    rm -f "$state/test-bundle/test-bundle.tar.gz" "$state/prepared/guest.qcow2"
    rm -f "$state/seed/user-data" "$state/seed/meta-data"
    rm -f "$state/seed/network-config"
    rmdir "$state/seed" 2>/dev/null || true
    echo "QEMU benchmark: status=$status; phase=$failed_phase; evidence=$state"
    if [ "$status" -ne 0 ] && [ -f "$console" ]; then tail -40 "$console" >&2; fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

git rev-parse HEAD >"$state/source-commit.txt"
git -C ../rust-fs-core rev-parse HEAD >"$state/core-commit.txt"
"qemu-system-$arch" --version >"$state/qemu-version.txt"
python3 - "$state" <<'PY'
import json, os, pathlib, platform, subprocess, sys
root = pathlib.Path(sys.argv[1])
(root / "host.json").write_text(json.dumps({
    "host": platform.platform(), "cpu_count": os.cpu_count(),
    "run_id": os.getenv("GITHUB_RUN_ID"), "job": os.getenv("GITHUB_JOB"),
    "runner_name": os.getenv("BENCH_RUNNER_NAME", "local"),
    "source_commit": (root / "source-commit.txt").read_text().strip(),
    "core_commit": (root / "core-commit.txt").read_text().strip(),
    "source_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], text=True).strip()),
}, indent=2) + "\n")
PY
python3 "$repo/scripts/ci-qemu-artifacts.py" keys "$repo" "$arch" "$workload" >"$state/cache-keys.json"
test_key="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["tests"])' "$state/cache-keys.json")"
guest_key="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["guest"])' "$state/cache-keys.json")"
mkdir -p "$state/test-bundle" "$state/prepared"
prepared_hit=false
guest_restore_started="$(date +%s)"
progress 'restoring environment-keyed prepared guest image'
if bash "$repo/scripts/ci-qemu-cache.sh" pull guest "$arch" "$guest_key" "$state/prepared"; then
    [ "$(cat "$state/prepared/guest-cache-key.txt")" = "$guest_key" ] || { echo 'guest cache key mismatch' >&2; exit 1; }
    base="$state/prepared/guest.qcow2"
    prepared_hit=true
    printf 'guest\thit\t%s\n' "$guest_key" >>"$state/cache-events.tsv"
else
    cache_status=$?
    [ "$cache_status" = 3 ] || exit "$cache_status"
    python3 "$repo/scripts/ci-qemu-artifacts.py" policy "$action" "$arch" "$accel" "$(uname -m)" false
    read -r image image_hash cloud_build < <(python3 "$repo/scripts/ci-qemu-artifacts.py" image "$arch")
    base="$repo/tmp/qemu-images/$image"
    progress 'download and verify pinned guest image'
    if [ ! -f "$base" ]; then
        curl -fL --retry 3 --connect-timeout 15 --max-time 600 \
            "https://cloud.debian.org/images/cloud/bookworm/$cloud_build/$image" -o "$base.partial"
        mv "$base.partial" "$base"
    fi
    python3 - "$base" "$image_hash" <<'PY'
import hashlib, sys
with open(sys.argv[1], "rb") as image:
    digest = hashlib.file_digest(image, "sha512").hexdigest()
if digest != sys.argv[2]:
    sys.exit("Debian cloud image SHA512 mismatch")
PY
    printf 'guest\tmiss\t%s\n' "$guest_key" >>"$state/cache-events.tsv"
fi
printf '%s\n' "$(( $(date +%s) - guest_restore_started ))" >"$state/guest-image-seconds.txt"
if [ "$action" != serve ]; then
progress 'restoring source-keyed native test bundle'
build_started="$(date +%s)"
if bash "$repo/scripts/ci-qemu-cache.sh" pull tests "$arch" "$test_key" "$state/test-bundle"; then
    [ "$(cat "$state/test-bundle/test-cache-key.txt")" = "$test_key" ] || { echo 'test cache key mismatch' >&2; exit 1; }
    printf 'tests\thit\t%s\n' "$test_key" >>"$state/cache-events.tsv"
else
    cache_status=$?
    [ "$cache_status" = 3 ] || exit "$cache_status"
    progress 'compiling target binaries natively in Debian 12; QEMU is not running'
    bash "$repo/scripts/ci-qemu-build.sh" "$arch" "$workload" "$state/test-bundle" \
        2>&1 | tee "$state/host-build.log"
    printf '%s\n' "$test_key" >"$state/test-bundle/test-cache-key.txt"
    bash "$repo/scripts/ci-qemu-cache.sh" push tests "$arch" "$test_key" "$state/test-bundle" \
        test-bundle.json test-bundle.tar.gz test-cache-key.txt rustc.txt compile.time builder-image.txt build-host-arch.txt build-packages.txt
    printf 'tests\tmiss\t%s\n' "$test_key" >>"$state/cache-events.tsv"
fi
printf '%s\n' "$(( $(date +%s) - build_started ))" >"$state/host-build-seconds.txt"
for metadata in rustc.txt compile.time builder-image.txt build-host-arch.txt build-packages.txt test-cache-key.txt; do
    cp "$state/test-bundle/$metadata" "$state/$metadata"
done
fi
if "$preparation" && "$prepared_hit"; then
    progress 'prepared guest and test bundle are already available; no VM setup needed'
    exit 0
fi
qemu-img create -q -f qcow2 -F qcow2 -b "$base" "$disk" 12G
ssh-keygen -q -t ed25519 -N '' -f "$state/key"
mkdir "$state/seed"
cat >"$state/seed/user-data" <<EOF
#cloud-config
disable_root: false
ssh_pwauth: false
users:
  - name: root
    ssh_authorized_keys:
      - $(cat "$state/key.pub")
bootcmd:
  - |
EOF
sed 's/^/    /' "$repo/scripts/ci-qemu-boot.sh" >>"$state/seed/user-data"
cat >>"$state/seed/user-data" <<EOF
runcmd:
  - [sh, -c, 'echo EXT4_BENCHMARK_NETWORK; cat /etc/machine-id; ip -4 address; ip -4 route; cat /run/systemd/network/*.network; networkctl status --all --no-pager || true']
  - [touch, /run/ext4-benchmark-ready]
EOF
printf 'instance-id: ext4-benchmark-%s\nlocal-hostname: ext4-benchmark\n' "$(basename "$state")" >"$state/seed/meta-data"
cp "$repo/scripts/qemu-guest-network.yaml" "$state/seed/network-config"
if command -v cloud-localds >/dev/null; then
    cloud-localds --network-config="$state/seed/network-config" "$seed" "$state/seed/user-data" "$state/seed/meta-data"
elif command -v hdiutil >/dev/null; then
    hdiutil makehybrid -o "$seed" -iso -joliet -default-volume-name cidata "$state/seed" >"$state/seed.log" 2>&1
else
    echo 'install cloud-image-utils (cloud-localds) to create the NoCloud seed' >&2
    exit 1
fi
port="$(python3 - <<'PY'
import socket
with socket.socket() as connection:
    connection.bind(("127.0.0.1", 0))
    print(connection.getsockname()[1])
PY
)"
qemu_args
progress 'booting guest; waiting for cloud-init and SSH'
printf '%q ' "${command_args[@]}" >"$state/command.txt"
printf '\n' >>"$state/command.txt"
touch "$console"
"${command_args[@]}" >"$state/qemu.log" 2>&1 &
qemu_pid=$!
(
    sample=0
    while kill -0 "$qemu_pid" 2>/dev/null; do
        ps -o rss= -p "$qemu_pid" >>"$state/qemu-rss-kib.txt" || break
        if [ "$((sample % 30))" -eq 0 ]; then
            printf '[%s] heartbeat %s/%s elapsed=%ss phase=%s cpu/rss=%s\n' \
                "$(date -u +%FT%TZ)" "$arch" "$accel" "$(( $(date +%s) - started ))" \
                "$(cat "$state/phase")" "$(ps -o %cpu=,rss= -p "$qemu_pid")"
            [ ! -s "$console" ] || tail -1 "$console"
            # The CI VM jobs only: the runner's own load, CPU pressure and
            # cgroup throttling beside the guest, in the job log and in the
            # uploaded evidence. The pilot's runs are unchanged.
            if [ "$action" = serve ]; then
                bash "$repo/scripts/ci-host-pressure.sh" | tee -a "$state/host-pressure.txt" || true
            fi
        fi
        sample=$((sample + 1))
        sleep 1
    done
) &
sampler_pid=$!
ssh_args=(-F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none -i "$state/key"
    -o StrictHostKeyChecking=accept-new -o "UserKnownHostsFile=$state/known_hosts"
    -o GlobalKnownHostsFile=/dev/null -o ConnectTimeout=30 -o ServerAliveInterval=15
    -o ServerAliveCountMax=4 -p "$port" root@127.0.0.1)
boot_started="$(date +%s)"
boot_deadline_seconds=900
if [ "$accel" = tcg ]; then boot_deadline_seconds=3600; fi
printf 'boot_budget_seconds=%s\nssh_connect_seconds=30\nprobe_budget_seconds=120\n' \
    "$boot_deadline_seconds" >"$state/boot-policy.txt"
emergency_reported=false
ssh_reported=false
ready_check='test -f /run/ext4-benchmark-ready && systemctl is-active --quiet multi-user.target'
if [ "$arch" = aarch64 ]; then ready_check+=' && mountpoint -q /boot/efi'; fi
ready_command="printf 'EXT4_BENCHMARK_SSH_CONNECTED\n'; if $ready_check; then exit 0; fi;
    date -u; cat /proc/uptime; cloud-init status --long || true;
    systemctl --failed --no-pager;
    efi_source=\$(findmnt --fstab --target /boot/efi --noheadings --output SOURCE 2>/dev/null || true);
    printf 'EXT4_BENCHMARK_EFI_SOURCE=%s\\n' \"\$efi_source\";
    systemctl show boot-efi.mount -p What -p Result -p ActiveState -p ActiveEnterTimestampMonotonic;
    if [ -n \"\$efi_source\" ]; then
        case \"\$efi_source\" in PARTUUID=*) efi_device=/dev/disk/by-partuuid/\${efi_source#PARTUUID=} ;; *) efi_device=\$efi_source ;; esac;
        efi_unit=\$(systemd-escape --path --suffix=device \"\$efi_device\" 2>/dev/null || true);
        printf 'EXT4_BENCHMARK_EFI_DEVICE=%s\\n' \"\$efi_device\";
        if [ -n \"\$efi_unit\" ]; then systemctl show \"\$efi_unit\" -p Result -p ActiveState -p JobRunningTimeoutUSec; fi;
        readlink -e \"\$efi_device\" || true;
    fi;
    journalctl -b -o short-monotonic --no-pager -n 30 -u boot-efi.mount -u systemd-udevd.service \
        -u cloud-init.service -u cloud-config.service -u cloud-final.service -u ssh.service;
    exit 1"
while :; do
    kill -0 "$qemu_pid" 2>/dev/null || { cat "$state/qemu.log" >&2; exit 1; }
    if grep -q EXT4_BENCHMARK_BOOT_FAILED "$console"; then
        echo 'guest boot recovery failed; refusing to wait for the overall deadline' >&2
        exit 1
    fi
    if ! "$emergency_reported" && grep -q 'You are in emergency mode' "$console"; then
        progress 'guest entered emergency mode; cloud-init will retry the required EFI mount once'
        tail -30 "$console"
        emergency_reported=true
    fi
    remaining="$((boot_deadline_seconds - $(date +%s) + boot_started))"
    if [ "$remaining" -le 0 ]; then
        echo "guest boot exceeded $boot_deadline_seconds seconds" >&2
        tail -40 "$state/boot-probes.log" >&2
        exit 1
    fi
    probe_seconds=120
    if [ "$remaining" -lt "$probe_seconds" ]; then probe_seconds="$remaining"; fi
    if python3 "$repo/scripts/ci-qemu-probe.py" "$probe_seconds" \
        ssh "${ssh_args[@]}" "$ready_command" >>"$state/boot-probes.log" 2>>"$state/ssh.log"; then
        if [ "$(( $(date +%s) - boot_started ))" -le "$boot_deadline_seconds" ]; then break; fi
        continue
    fi
    if ! "$ssh_reported" && grep -q EXT4_BENCHMARK_SSH_CONNECTED "$state/boot-probes.log"; then
        progress 'SSH reached guest; waiting for cloud-init, normal boot target and required mounts'
        tail -20 "$state/boot-probes.log"
        ssh_reported=true
    fi
    remaining="$((boot_deadline_seconds - $(date +%s) + boot_started))"
    if [ "$remaining" -le 0 ]; then continue; fi
    if [ "$remaining" -lt 3 ]; then sleep "$remaining"; else sleep 3; fi
done
printf '%s\n' "$(( $(date +%s) - boot_started ))" >"$state/boot-seconds.txt"
progress "guest ready after $(cat "$state/boot-seconds.txt")s; checking architecture"
actual_arch="$(ssh "${ssh_args[@]}" uname -m)"
[ "$actual_arch" = "$arch" ] || { echo "wrong guest architecture: expected $arch, got $actual_arch" >&2; exit 1; }
# One phase per SSH call after readiness, so a failed call is named by the
# status line and result.json rather than left as "checking architecture".
progress "guest architecture is $actual_arch; packing the repository payload"
COPYFILE_DISABLE=1 tar --no-xattrs -czf "$state/payload.tar.gz" --exclude=.git --exclude=target --exclude=tmp \
    --exclude=.vm-share --exclude=.vagrant --exclude='test-disks/*.img' \
    -C "$repo" .
progress "sending the repository payload ($(( $(wc -c <"$state/payload.tar.gz") )) bytes)"
ssh "${ssh_args[@]}" 'rm -rf /repo /rust-fs-core /benchmark-target; mkdir -p /repo /rust-fs-core; tar -xzf - -C /repo' <"$state/payload.tar.gz"
COPYFILE_DISABLE=1 tar --no-xattrs -czf "$state/core-payload.tar.gz" --exclude=.git --exclude=target --exclude=tmp \
    -C "$repo/../rust-fs-core" .
progress "sending the rust-fs-core payload ($(( $(wc -c <"$state/core-payload.tar.gz") )) bytes)"
ssh "${ssh_args[@]}" 'tar -xzf - -C /rust-fs-core' <"$state/core-payload.tar.gz"
if [ "$action" = serve ]; then
    if ! "$prepared_hit"; then
        # The same setup a full preparation runs (scripts/ci-qemu-guest.sh),
        # in this live guest only: nothing here is published.
        progress 'cold guest: installing the full preparation tools in this live guest'
        ssh "${ssh_args[@]}" "cd /repo; export FLTH_GUEST=1 DEBIAN_FRONTEND=noninteractive \
            RUSTUP_HOME=/var/lib/fs-ext4-rust/rustup CARGO_HOME=/var/lib/fs-ext4-rust/cargo; \
            apt-get update && apt-get install -y time python3 binutils ripgrep ca-certificates && \
            bash scripts/vm-setup.sh && bash ../rust-fs-core/scripts/guest-rust-toolchain.sh" \
            2>&1 | tee "$state/cold-setup.log"
    fi
    # Short, because a socket path has a length limit: one connection is
    # shared by every call the command makes.
    control="/tmp/ext4-guest-$(basename "$state")"
    cat >"$state/ssh_config" <<EOF
Host guest
    HostName 127.0.0.1
    Port $port
    User root
    IdentityFile $state/key
    IdentitiesOnly yes
    IdentityAgent none
    BatchMode yes
    StrictHostKeyChecking yes
    UserKnownHostsFile $state/known_hosts
    GlobalKnownHostsFile /dev/null
    ConnectTimeout 30
    ServerAliveInterval 15
    ServerAliveCountMax 8
    ControlMaster auto
    ControlPath $control
    ControlPersist 900
EOF
    progress "guest ready; running beside it: ${serve_command[*]}"
    result=0
    workload_started="$(date +%s)"
    FS_EXT4_GUEST_SSH="$state/ssh_config" FS_EXT4_GUEST_ARCH="$arch" "${serve_command[@]}" || result=$?
    printf '%s\n' "$(( $(date +%s) - workload_started ))" >"$state/workload-seconds.txt"
    ssh -F "$state/ssh_config" -O exit guest 2>/dev/null || true
    progress "served command finished; status=$result"
    exit "$result"
fi
tar -cf - -C "$state/test-bundle" test-bundle.json test-bundle.tar.gz | \
    ssh "${ssh_args[@]}" 'mkdir -p /repo/tmp/test-bundle; tar -xf - -C /repo/tmp/test-bundle'
progress "running guest workload=$workload (live setup and prebuilt test output; no guest compilation)"
result=0
workload_budget=7200
if [ "$workload" = single ]; then
    workload_budget=600
    if "$preparation"; then workload_budget=1800; fi
fi
printf 'operation=%s\nworkload_budget_seconds=%s\n' "$action" "$workload_budget" >"$state/workload-policy.txt"
guest_operation=run
if "$preparation"; then guest_operation=prepare; fi
workload_started="$(date +%s)"
ssh "${ssh_args[@]}" "cd /repo; export FLTH_GUEST=1 BENCH_GUEST_KEY='$guest_key' BENCH_OPERATION='$guest_operation'; timeout --signal=TERM --kill-after=30s '$workload_budget' bash scripts/ci-qemu-guest.sh '$arch' '$workload'" \
    2>&1 | tee "$state/workload.log" || result=$?
printf '%s\n' "$(( $(date +%s) - workload_started ))" >"$state/workload-seconds.txt"
progress "collecting guest results; workload status=$result"
if [ "$result" = 124 ]; then
    printf 'Guest workload exceeded its %ss deadline; this is a timeout, not evidence of a guest crash.\n' "$workload_budget" >&2
fi
collected=false
for collection_attempt in 1 2 3; do
    if ssh "${ssh_args[@]}" 'tar -czf - -C /repo/tmp/qemu-results .' >"$state/guest-results.tar.gz"; then
        collected=true
        break
    else
        collection_status=$?
        [ "$collection_status" = 255 ] || break
        printf 'Result transport failed (attempt %s/3); retrying read-only collection, not tests\n' "$collection_attempt" >&2
        sleep 1
    fi
done
if ! "$collected"; then result=1; fi
if [ -s "$state/guest-results.tar.gz" ]; then tar -xzf "$state/guest-results.tar.gz" -C "$state"; fi
if [ "$result" = 0 ] && "$preparation" && ! "$prepared_hit"; then
    progress 'cleanly shutting down prepared guest before publishing reusable disk'
    printf -v cleanup_command 'FLTH_GUEST=1 bash -c %q' "$(cat "$repo/scripts/ci-qemu-image-clean.sh")"
    ssh "${ssh_args[@]}" "$cleanup_command" \
        >"$state/image-cleanup.log" 2>&1 || true
    grep -q EXT4_BENCHMARK_IMAGE_CLEANED "$state/image-cleanup.log" || { cat "$state/image-cleanup.log" >&2; exit 1; }
    for _ in $(seq 1 120); do
        kill -0 "$qemu_pid" 2>/dev/null || break
        sleep 1
    done
    if kill -0 "$qemu_pid" 2>/dev/null; then echo 'guest did not power off; refusing to publish a live disk' >&2; exit 1; fi
    wait "$qemu_pid"
    qemu_pid=''
    qemu-img convert -p -c -O qcow2 "$disk" "$state/prepared/guest.qcow2"
    printf '%s\n' "$guest_key" >"$state/prepared/guest-cache-key.txt"
    bash "$repo/scripts/ci-qemu-cache.sh" push guest "$arch" "$guest_key" "$state/prepared" \
        guest.qcow2 guest-cache-key.txt
fi
exit "$result"
