#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pilot="$repo/scripts/ci-qemu-benchmark.sh"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

python3 - "$repo" "$scratch" <<'PY'
import os
import pathlib
import subprocess
import sys

root, scratch = map(pathlib.Path, sys.argv[1:])
tools = scratch / 'boot-tools'
tools.mkdir()
for name, body in {
    'systemctl': '''printf '%s\\n' "$*" >> "$BOOT_CALLS"
case "$1" in
is-active) test "$BOOT_MODE" != normal ;;
start) test "$BOOT_MODE" != broken ;;
esac''',
    'mountpoint': 'test "$BOOT_MODE" != broken',
    'udevadm': 'exit 0',
    'journalctl': 'echo "boot-efi.mount: dependency failed"',
    'timeout': 'shift; exec "$@"',
}.items():
    path = tools / name
    path.write_text('#!/bin/sh\n' + body + '\n')
    path.chmod(0o755)
for mode in ('normal', 'recoverable', 'broken'):
    calls = scratch / (mode + '.calls')
    environment = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ['PATH'],
                       BOOT_MODE=mode, BOOT_CALLS=str(calls))
    result = subprocess.run(['bash', str(root / 'scripts/ci-qemu-boot.sh')],
                            env=environment, text=True, capture_output=True)
    assert (result.returncode == 0) == (mode != 'broken'), (mode, result.stdout, result.stderr)
    commands = calls.read_text()
    assert ('default --no-block' in commands) == (mode == 'recoverable'), (mode, commands)
    assert ('start boot-efi.mount' in commands) == (mode != 'normal'), (mode, commands)
    if mode == 'broken':
        assert 'EXT4_BENCHMARK_BOOT_FAILED' in result.stdout + result.stderr
PY

python3 - "$repo" "$scratch" <<'PY'
import os
import pathlib
import subprocess
import sys

root, scratch = map(pathlib.Path, sys.argv[1:])
host = (root / 'scripts/ci-qemu-benchmark.sh').read_text()
bootstrap = host[host.index('ssh_args=('):host.index('actual_arch=')]
tools = scratch / 'slow-boot-tools'
tools.mkdir()
for name, body in {
    'date': '''import os, pathlib
print(pathlib.Path(os.environ['BOOT_CLOCK']).read_text())''',
    'ssh': '''import os, pathlib, sys
clock = pathlib.Path(os.environ['BOOT_CLOCK'])
elapsed = int(clock.read_text()) + 400
clock.write_text(str(elapsed))
if 'ConnectTimeout=30' not in sys.argv:
    sys.exit(255)
print('EXT4_BENCHMARK_SSH_CONNECTED')
sys.exit(0 if elapsed >= 1000 and os.environ['BOOT_NEVER_READY'] == '0' else 1)''',
}.items():
    executable = tools / name
    executable.write_text('#!/usr/bin/env python3\n' + body + '\n')
    executable.chmod(0o755)
(tools / 'sleep').write_text('#!/bin/sh\nexit 0\n')
(tools / 'sleep').chmod(0o755)
for accelerator, never_ready, expected in [('tcg', '0', 0), ('kvm', '0', 1), ('tcg', '1', 1)]:
    state = scratch / f'boot-{accelerator}-{never_ready}'
    state.mkdir()
    (state / 'clock').write_text('0')
    (state / 'console.log').touch()
    environment = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ['PATH'],
                       BOOT_CLOCK=str(state / 'clock'), BOOT_NEVER_READY=never_ready)
    program = '''set -euo pipefail
repo="$1"; state="$2"; accel="$3"; qemu_pid="$4"; arch=aarch64
console="$state/console.log"
port=2222
progress() { printf '%s\\n' "$*"; }
''' + bootstrap
    result = subprocess.run(['bash', '-c', program, '--', str(root), str(state),
                             accelerator, str(os.getpid())], env=environment,
                            capture_output=True, text=True, timeout=10)
    assert result.returncode == expected, (
        'a progressing TCG boot must survive 900 seconds and permit a slow SSH handshake',
        accelerator, never_ready, result.returncode, result.stdout, result.stderr)
    if expected:
        limit = 1800 if accelerator == 'tcg' else 900
        assert f'guest boot exceeded {limit} seconds' in result.stderr, result.stderr
    else:
        assert int((state / 'boot-seconds.txt').read_text()) == 1200

probe = root / 'scripts/ci-qemu-probe.py'
for command, expected in [('print("probe passed")', 0), ('raise SystemExit(17)', 17)]:
    result = subprocess.run([sys.executable, str(probe), '2', sys.executable, '-c', command],
                            capture_output=True, text=True, timeout=5)
    assert result.returncode == expected, (command, result)
result = subprocess.run([sys.executable, str(probe), '0.05', sys.executable,
                         '-c', 'import time; time.sleep(10)'],
                        capture_output=True, text=True, timeout=2)
assert result.returncode == 124, result
assert 'guest boot probe exceeded' in result.stderr, result.stderr
for invalid in ('0', '-1', 'nan', 'inf', 'invalid'):
    result = subprocess.run([sys.executable, str(probe), invalid, sys.executable, '-c', 'pass'])
    assert result.returncode == 2, (invalid, result.returncode)
PY

bash "$pilot" plan x86_64 tcg single >"$scratch/single-x64"
bash "$pilot" plan aarch64 tcg single >"$scratch/single-arm64"
test "$(bash "$repo/scripts/ci-qemu-guest.sh" plan single)" = basic
python3 - "$repo" <<'PY'
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
guest = (root / "scripts/ci-qemu-guest.sh").read_text()
host = (root / "scripts/ci-qemu-benchmark.sh").read_text()
assert 'tee "$evidence/tests.log"' in guest
assert 'tee "$state/workload.log"' in host
assert 'heartbeat' in host
assert 'scripts/ci-qemu-boot.sh' in host
assert 'mountpoint -q /boot/efi' in host
assert 'EXT4_BENCHMARK_EFI_SOURCE' in host
assert 'JobRunningTimeoutUSec' in host
assert 'ActiveEnterTimestampMonotonic' in host
assert 'systemctl is-active --quiet multi-user.target' in host
assert "inputs.workload || 'single'" in (root / '.github/workflows/qemu-benchmark.yml').read_text()
PY

# The defaults are what every ordinary run uses, so they are graded with the
# experiment overrides removed; the overrides are graded after them.
plan() { env -u QEMU_TCG_CPU -u QEMU_TCG_ACCEL bash "$pilot" plan "$@"; }
plan x86_64 tcg >"$scratch/x64"
plan aarch64 tcg >"$scratch/arm"
plan x86_64 kvm >"$scratch/kvm"
QEMU_TCG_CPU='max,pauth-impdef=on' QEMU_TCG_ACCEL='tcg,thread=multi,tb-size=1024' \
    bash "$pilot" plan aarch64 tcg >"$scratch/arm-tuned"
QEMU_TCG_CPU='max,pauth-impdef=on' QEMU_TCG_ACCEL='tcg,thread=multi,tb-size=1024' \
    bash "$pilot" plan x86_64 kvm >"$scratch/kvm-tuned"
python3 - "$scratch" <<'PY'
import pathlib
import shlex
import sys

root = pathlib.Path(sys.argv[1])
x64, arm, kvm = [shlex.split((root / name).read_text()) for name in ("x64", "arm", "kvm")]
assert x64[0] == "qemu-system-x86_64"
assert arm[0] == "qemu-system-aarch64"
for command in (x64, arm):
    assert command[command.index("-accel") + 1] == "tcg,thread=multi"
    assert command[command.index("-cpu") + 1] == "max"
    assert "kvm" not in command
assert arm[arm.index("-machine") + 1] == "virt"
assert "-bios" in arm
assert kvm[kvm.index("-accel") + 1] == "kvm"
assert kvm[kvm.index("-cpu") + 1] == "host"
tuned = shlex.split((root / "arm-tuned").read_text())
assert tuned[tuned.index("-cpu") + 1] == "max,pauth-impdef=on"
assert tuned[tuned.index("-accel") + 1] == "tcg,thread=multi,tb-size=1024"
# The overrides are for emulation only: a hardware-accelerated guest ignores them.
kvm_tuned = shlex.split((root / "kvm-tuned").read_text())
assert kvm_tuned == kvm
for command in (x64, arm, kvm):
    assert command[command.index("-m") + 1] == "2048"
    assert command[command.index("-smp") + 1] == "2"
    assert "hostfwd=tcp:127.0.0.1:2222-:22" in " ".join(command)
PY

for invalid in 'aarch64 kvm' 'x86_64 auto' 'ppc64 tcg'; do
    read -r invalid_arch invalid_accel <<<"$invalid"
    if bash "$pilot" plan "$invalid_arch" "$invalid_accel" >"$scratch/error" 2>&1; then
        echo "FAIL: unsupported plan accepted: $invalid" >&2
        exit 1
    fi
done

python3 - "$repo/.github/workflows/qemu-benchmark.yml" <<'PY'
import pathlib
import re
import sys

workflow = pathlib.Path(sys.argv[1]).read_text()
for name in ("hosted-x64", "vps-x64", "hosted-arm64", "vps-arm64"):
    job = re.search(rf"^  {name}:\n(.*?)(?=^  \S|\Z)", workflow, re.M | re.S).group(1)
    assert not re.search(r"^    needs:", job, re.M), f"{name} must not wait for a hosted bootstrap"
    label = "antimatter-vps-linux-x64" if name.startswith("vps-") else "ubuntu-24.04"
    assert f"    runs-on: {label}\n" in job, f"{name} must measure its intended placement"
    for identity in ("github.actor", "github.triggering_actor", "github.event.sender.login",
                     "github.event.pull_request.user.login"):
        assert f"{identity} == 'christhomas'" in job, f"{name} must check {identity}"
PY

python3 - "$repo" <<'PY'
import os
import pathlib
import re
import shlex
import subprocess
import sys
import tempfile

root = pathlib.Path(sys.argv[1])
guest = root / "scripts/ci-qemu-guest.sh"
builder = (root / "test-disks/guest-build-images.sh").read_text()
targets = re.findall(r"^\s+(\w+)\) build_", builder, re.M)
full = subprocess.check_output(["bash", str(guest), "plan", "full"], text=True).split()
smoke = subprocess.check_output(["bash", str(guest), "plan", "smoke"], text=True).split()
assert full == targets, (full, targets)
assert smoke == ["basic"]
with tempfile.TemporaryDirectory() as temporary:
    link = pathlib.Path(temporary) / "repository-link"
    link.symlink_to(root, target_is_directory=True)
    entry = next(line for line in guest.read_text().splitlines() if line.startswith("cd "))
    command = shlex.split(entry)
    assert command[-1] == "/repo", command
    command[-1] = str(link)
    environment = os.environ.copy()
    environment.pop("FS_EXT4_TEST_TMPDIR", None)
    scratch = subprocess.check_output(
        ["bash", "-c", shlex.join(command) + "; bash scripts/test.sh --print-temp-dir"],
        env=environment, text=True,
    ).strip()
    assert pathlib.Path(scratch).is_relative_to(root.resolve()), (
        "guest scratch path must use Cargo's physical repository path", scratch, str(root.resolve()))
PY

python3 - "$repo/scripts/report-qemu-benchmark.py" "$scratch" <<'PY'
import json
import pathlib
import subprocess
import sys

report = sys.argv[1]
root = pathlib.Path(sys.argv[2]) / "measurements"
root.mkdir()
jobs = []
passing_names = "first\nsecond\n" + "".join(f"case_{index}\n" for index in range(438))
test_log = "".join(f"test {name} ... ok\n" for name in passing_names.splitlines())
test_log += "test result: ok. 440 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s\n"
for name, arch in (("hosted-x64", "x86_64"), ("vps-x64", "x86_64"),
                   ("hosted-arm64", "aarch64"), ("vps-arm64", "aarch64")):
    trial = root / name
    trial.mkdir()
    vps = name.startswith("vps")
    (trial / "result.json").write_text(json.dumps({
        "guest_arch": arch, "accelerator": "tcg" if vps or arch == "aarch64" else "kvm",
        "workload": "smoke", "guest_memory_mib": 2048, "guest_cpus": 2, "build_mode": "host-prebuilt",
        "status": 0, "elapsed_seconds": 1800, "finished_unix": 3500,
        "job_ready_unix": 1100, "job_arrival_unix": 1705,
        "runner_environment": "self-hosted" if vps else "github-hosted",
        "selected_runner": "antimatter-vps-linux-x64" if vps else "ubuntu-24.04",
    }))
    (trial / "host.json").write_text(json.dumps({"job": name, "source_commit": "same", "core_commit": "same"}))
    (trial / "passing-tests.txt").write_text(passing_names)
    (trial / "tests.log").write_text(test_log)
    (trial / "rustc.txt").write_text(f"host: {arch}-unknown-linux-gnu\n")
    for metadata in ("builder-image.txt", "build-packages.txt", "test-cache-key.txt", "guest-packages.txt"):
        (trial / metadata).write_text("matching-input\n")
    jobs.append({"name": name, "conclusion": "success", "steps": [{"started_at": "1970-01-01T00:28:20Z"}]})
jobs_path = root.parent / "jobs.json"
jobs_path.write_text(json.dumps({"jobs": jobs}))
run_path = root.parent / "run.json"
run_path.write_text(json.dumps({"created_at": "1970-01-01T00:16:40Z", "head_sha": "pull-request-head"}))

def check(success, message, source_commit="same"):
    result = subprocess.run([sys.executable, report, str(root), str(jobs_path), str(run_path), "smoke", source_commit],
                            capture_output=True, text=True)
    assert (result.returncode == 0) == success, result.stdout + result.stderr
    assert message in result.stdout, result.stdout

check(True, "| 700 | 1800 | 2500 | passed |")
for trial in root.iterdir():
    identity_path = trial / "host.json"
    identity = json.loads(identity_path.read_text())
    identity["source_commit"] = "stale"
    identity_path.write_text(json.dumps(identity))
check(False, "does not match workflow source")
for trial in root.iterdir():
    identity_path = trial / "host.json"
    identity = json.loads(identity_path.read_text())
    identity["source_commit"] = "same"
    identity_path.write_text(json.dumps(identity))
for trial_name in ("hosted-arm64", "vps-arm64"):
    result_path = root / trial_name / "result.json"
    result = json.loads(result_path.read_text())
    result["workload"] = "full"
    result_path.write_text(json.dumps(result))
check(False, "does not match requested workload smoke")
for trial_name in ("hosted-arm64", "vps-arm64"):
    result_path = root / trial_name / "result.json"
    result = json.loads(result_path.read_text())
    result["workload"] = "smoke"
    result["guest_arch"] = "x86_64"
    result_path.write_text(json.dumps(result))
check(False, "expected aarch64 / tcg")
for trial_name in ("hosted-arm64", "vps-arm64"):
    result_path = root / trial_name / "result.json"
    result = json.loads(result_path.read_text())
    result["guest_arch"] = "aarch64"
    result_path.write_text(json.dumps(result))
accelerator_path = root / "hosted-x64" / "result.json"
accelerator_original = accelerator_path.read_text()
accelerator_result = json.loads(accelerator_original)
accelerator_result["accelerator"] = "tcg"
accelerator_path.write_text(json.dumps(accelerator_result))
check(False, "expected x86_64 / kvm")
accelerator_path.write_text(accelerator_original)
check(False, "missing workflow source commit", source_commit="")
identity_path = root / "hosted-x64" / "host.json"
identity_original = identity_path.read_text()
identity = json.loads(identity_original)
identity["job"] = "unexpected-x64"
identity_path.write_text(json.dumps(identity))
check(False, "unexpected trial identity")
identity_path.write_text(identity_original)
target = root / "vps-arm64" / "result.json"
original = target.read_text()
value = json.loads(original)
value["selected_runner"] = "ubuntu-24.04"
target.write_text(json.dumps(value))
check(False, "NOT a VPS measurement")
target.write_text(original)
value = json.loads(original)
del value["build_mode"]
target.write_text(json.dumps(value))
check(False, "missing build_mode")
target.write_text(original)
(root / "vps-arm64" / "passing-tests.txt").write_text("different\n")
check(False, "passing-tests.txt differs")
(root / "vps-arm64" / "passing-tests.txt").write_text(passing_names)
value = json.loads(original)
value["status"] = 137
target.write_text(json.dumps(value))
check(False, "workload failed")
target.rename(target.with_suffix(".missing"))
check(False, "no measurements")
PY

python3 "$repo/scripts/test-ci-qemu-artifacts.py"
python3 "$repo/scripts/test-ci-qemu-environment.py"
python3 "$repo/scripts/test-report-qemu-benchmark.py"

echo 'PASS: architecture, accelerator, queue accounting, fallback and matching-workload checks'
