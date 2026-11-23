#!/usr/bin/env python3
import importlib.util
import json
import os
import pathlib
import re
import shutil
import subprocess
import tempfile
import textwrap
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
DIGEST = "a" * 64


class EnvironmentCommands(unittest.TestCase):
    def test_full_preparation_has_a_workload_specific_job_deadline(self):
        workflow = (ROOT / ".github/workflows/qemu-prepare.yml").read_text()
        preparation = workflow.split("  prepare:\n", 1)[1]
        self.assertIn("timeout-minutes: ${{ inputs.workload == 'full' && 180 || 45 }}", preparation)

    def test_warm_restore_recovers_ownership_before_its_separate_upload(self):
        workflow = (ROOT / ".github/workflows/qemu-prepare.yml").read_text()
        recovery = re.search(
            r"      - name: Restore warm-restore diagnostic ownership\n(.*?)"
            r"      - name: Preserve warm-restore evidence\n", workflow, re.S)
        self.assertIsNotNone(recovery)
        self.assertIn("if: always()", recovery[1])
        self.assertIn("run: bash scripts/ci-qemu-diagnostics.sh", recovery[1])

    def diagnostic_recovery_program(self):
        action = (ROOT / ".github/actions/qemu-benchmark/action.yml").read_text()
        recovery = re.search(
            r"    - name: Restore diagnostic ownership after interruption\n(.*?)"
            r"    - name: Preserve measurements and failures\n", action, re.S)
        self.assertIsNotNone(recovery, "interrupted containers must not strand root-owned diagnostics")
        self.assertIn("if: always()", recovery[1])
        self.assertIn("run: bash scripts/ci-qemu-diagnostics.sh", recovery[1])
        return "bash scripts/ci-qemu-diagnostics.sh"

    def execute_diagnostic_recovery(self, failure=0):
        shutil.copyfile(ROOT / "scripts/ci-qemu-diagnostics.sh",
                        self.repo / "scripts/ci-qemu-diagnostics.sh")
        sudo = self.root / "tools/sudo"
        sudo.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
arguments = sys.argv[1:]
with open(os.environ['SUDO_TRACE'], 'a') as trace:
    trace.write(json.dumps(arguments) + '\\n')
assert arguments[:3] == ['-n', 'chown', '-hR'], arguments
assert arguments[-2] == '--', arguments
if int(os.environ['SUDO_FAILURE']):
    sys.exit(int(os.environ['SUDO_FAILURE']))
directory = pathlib.Path(arguments[-1])
directory.chmod(0o700)
for current, directories, files in os.walk(directory):
    for name in directories:
        child = pathlib.Path(current) / name
        if not child.is_symlink():
            child.chmod(0o700)
''')
        sudo.chmod(0o755)
        return subprocess.run(
            ["bash", "-c", self.diagnostic_recovery_program()], cwd=self.repo,
            env=dict(self.env, SUDO_TRACE=str(self.root / "sudo.jsonl"),
                     SUDO_FAILURE=str(failure)), capture_output=True, text=True,
            check=False, timeout=5)

    def test_interrupted_container_diagnostics_are_recovered_before_upload(self):
        evidence = self.repo / "tmp/qemu-benchmark/interrupted"
        evidence.mkdir(parents=True)
        console = evidence / "console.log"
        console.write_text("last progress before the job was cancelled\n")
        evidence.chmod(0)
        self.addCleanup(evidence.chmod, 0o700)
        result = self.execute_diagnostic_recovery()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(console.read_text(), "last progress before the job was cancelled\n")
        calls = [json.loads(line) for line in (self.root / "sudo.jsonl").read_text().splitlines()]
        self.assertEqual(calls, [["-n", "chown", "-hR", f"{os.getuid()}:{os.getgid()}",
                                 "--", "tmp/qemu-benchmark"]])

    def test_diagnostic_recovery_ignores_missing_and_symlinked_roots(self):
        outside = self.root / "outside"
        outside.mkdir()
        temporary = self.repo / "tmp"
        temporary.mkdir()
        (temporary / "qemu-images").symlink_to(outside, target_is_directory=True)
        result = self.execute_diagnostic_recovery()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "sudo.jsonl").exists())

    def test_diagnostic_ownership_failures_remain_failures(self):
        (self.repo / "tmp/qemu-benchmark").mkdir(parents=True)
        result = self.execute_diagnostic_recovery(failure=33)
        self.assertEqual(result.returncode, 33, result.stderr)

    def test_preparation_bootstrap_works_without_local_bin_on_path(self):
        workflow = (ROOT / ".github/workflows/qemu-prepare.yml").read_text()
        programs = [textwrap.dedent(match[2]) for match in re.finditer(
            r"(?m)^([ ]+)run: \|\n((?:\1  [^\n]*\n|[ \t]*\n)+)", workflow)
            if "bash scripts/ci-install-chore.sh" in match[2]]
        self.assertEqual(len(programs), 2)
        for index, program in enumerate(programs):
            with self.subTest(bootstrap=index):
                home = self.root / f"home-{index}"
                binary = home / ".local/bin/chore"
                binary.parent.mkdir(parents=True)
                binary.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$CHORE_TRACE"\n')
                binary.chmod(0o755)
                installer = self.repo / "scripts/ci-install-chore.sh"
                installer.write_text('#!/bin/sh\nprintf "%s\\n" "$HOME/.local/bin" >> "$GITHUB_PATH"\n')
                (self.repo / "scripts/ci-qemu-environment.sh").write_text("#!/bin/sh\nexit 0\n")
                trace = self.root / f"chore-{index}.log"
                path_file = self.root / f"github-path-{index}"
                result = subprocess.run(["/bin/bash", "-e", "-c", program], cwd=self.repo,
                                        env=dict(os.environ, HOME=str(home), PATH=os.defpath,
                                                 GITHUB_PATH=str(path_file), CHORE_TRACE=str(trace)),
                                        capture_output=True, text=True, check=False, timeout=5)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(trace.read_text(), "siblings\n")
                self.assertEqual(path_file.read_text(), str(binary.parent) + "\n")

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = pathlib.Path(self.temporary.name)
        self.repo = self.root / "repo"
        for name in ("scripts/ci-qemu-environment.sh", "scripts/ci-qemu-artifacts.py",
                     "scripts/ci-qemu-runner.sh", "scripts/ci-qemu-cache.sh",
                     "scripts/qemu-pilot-environment.json", "scripts/qemu-runner/Dockerfile",
                     "scripts/qemu-runner/setup.sh", "scripts/qemu-runner/packages.txt",
                     "rust-toolchain.toml"):
            destination = self.repo / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, destination)
        core = self.root / "rust-fs-core"
        (core / "scripts").mkdir(parents=True)
        (core / "scripts/guest-rust-toolchain.sh").write_text("pinned setup")
        spec = importlib.util.spec_from_file_location("runner_artifacts", self.repo / "scripts/ci-qemu-artifacts.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.key = module.runner_recipe(self.repo, core, "x86_64")["key"]
        tools = self.root / "tools"
        tools.mkdir()
        docker = tools / "docker"
        docker.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
arguments = sys.argv[1:]
with open(os.environ['DOCKER_TRACE'], 'a') as trace:
    trace.write(json.dumps(arguments) + '\\n')
if arguments[0] == 'info':
    print('amd64')
elif arguments[:2] == ['context', 'inspect']:
    print('unix:///fixture/docker.sock')
elif arguments[0] == 'login':
    sys.stdin.read()
elif arguments[0] == 'pull':
    if os.getenv('PULL_ERROR'):
        sys.exit(os.environ['PULL_ERROR'])
elif arguments[:2] == ['image', 'inspect']:
    template = arguments[arguments.index('--format') + 1]
    if 'runner-key' in template:
        print(os.getenv('ACTUAL_KEY', os.environ['RECIPE_KEY']))
    elif '.Architecture' in template:
        print(os.getenv('ACTUAL_ARCH', 'amd64'))
    elif '.RepoDigests' in template:
        print(json.dumps(['ghcr.io/example/repo/qemu-pilot@sha256:' + 'a' * 64]))
    else:
        print('sha256:' + 'a' * 64)
elif arguments[0] == 'build':
    context = pathlib.Path(arguments[-1])
    pathlib.Path(os.environ['BUILD_CONTEXT']).write_text(json.dumps(sorted(item.name for item in context.iterdir())))
elif arguments[0] == 'run':
    sys.exit(int(os.getenv('RUN_STATUS', '0')))
''')
        docker.chmod(0o755)
        self.env = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"],
                        BENCH_CACHE_REGISTRY="ghcr.io/example/repo/qemu-pilot", RECIPE_KEY=self.key,
                        DOCKER_TRACE=str(self.root / "docker.jsonl"),
                        BUILD_CONTEXT=str(self.root / "context.json"),
                        GH_TOKEN="test-only-credential", GITHUB_ACTOR="example")

    def execute(self, script, *arguments, **overrides):
        return subprocess.run(["bash", str(self.repo / "scripts" / script), *arguments],
                              env=dict(self.env, **overrides), capture_output=True, text=True,
                              check=False, timeout=10)

    def calls(self):
        return [json.loads(line) for line in (self.root / "docker.jsonl").read_text().splitlines()]

    def test_build_context_excludes_sources_and_credentials(self):
        result = self.execute("ci-qemu-environment.sh", "build", "amd64")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads((self.root / "context.json").read_text()),
                         ["Dockerfile", "core-toolchain.sh", "environment.json", "packages.txt",
                          "rust-toolchain.toml", "setup.sh"])
        self.assertNotIn("test-only-credential", json.dumps(self.calls()))
        self.assertIn("runner-x86_64-" + self.key, result.stdout)

    def test_resolve_pins_digest_and_rejects_wrong_recipe_or_arch(self):
        result = self.execute("ci-qemu-environment.sh", "resolve", "x86_64")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "ghcr.io/example/repo/qemu-pilot@sha256:" + DIGEST)
        for override in ({"ACTUAL_ARCH": "arm64"}, {"ACTUAL_KEY": "wrong"}):
            result = self.execute("ci-qemu-environment.sh", "resolve", "x86_64", **override)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("mismatch", result.stderr)

    def test_resolve_never_builds_a_missing_environment(self):
        result = self.execute("ci-qemu-environment.sh", "resolve", "x86_64", PULL_ERROR="manifest unknown")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("build and publish it locally first", result.stderr)
        self.assertFalse(any(call[0] in ("build", "run") for call in self.calls()))

    def test_publish_preserves_existing_recipe_and_propagates_auth_errors(self):
        result = self.execute("ci-qemu-environment.sh", "publish", "x86_64")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(call[0] == "push" for call in self.calls()))
        result = self.execute("ci-qemu-environment.sh", "publish", "x86_64", PULL_ERROR="unauthorized")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(call[0] == "push" for call in self.calls()))

    def test_publish_only_pushes_absent_recipe(self):
        result = self.execute("ci-qemu-environment.sh", "publish", "x86_64", PULL_ERROR="manifest unknown")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len([call for call in self.calls() if call[0] == "push"]), 1)

    def test_preparation_reuses_environment_without_build_or_push(self):
        result = self.execute("ci-qemu-environment.sh", "prepare", "x86_64")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(call[0] in ("build", "push") for call in self.calls()))

    def test_preparation_does_not_build_on_auth_failure(self):
        result = self.execute("ci-qemu-environment.sh", "prepare", "x86_64", PULL_ERROR="unauthorized")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(call[0] == "build" for call in self.calls()))

    def run_command(self, **overrides):
        result = self.execute("ci-qemu-runner.sh", "run", "aarch64", "tcg", "single", RUN_STATUS="17", **overrides)
        self.assertEqual(result.returncode, 17, result.stderr)
        return [call for call in self.calls() if call[0] == "run"][-1]

    def test_runtime_container_has_no_docker_socket_or_kvm_for_tcg(self):
        command = self.run_command(BENCH_RUNNER_ENVIRONMENT="self-hosted")
        self.assertNotIn("--privileged", command)
        self.assertNotIn("--device", command)
        self.assertNotIn("docker.sock", json.dumps(command))
        self.assertNotIn("test-only-credential", json.dumps(command))
        self.assertIn("GH_TOKEN", command)
        self.assertIn("ghcr.io/example/repo/qemu-pilot@sha256:" + DIGEST, command)

    def test_the_runner_container_is_never_limited_by_this_script(self):
        for environment in ("self-hosted", "github-hosted", ""):
            command = self.run_command(BENCH_RUNNER_ENVIRONMENT=environment)
            self.assertFalse([argument for argument in command
                              if argument.startswith(("--cpus", "--memory"))])

    def test_local_cache_roundtrip_needs_no_registry_or_credentials(self):
        source, target = self.root / "source", self.root / "target"
        source.mkdir()
        (source / "guest.qcow2").write_bytes(b"fixture disk")
        environment = {"BENCH_CACHE_DIRECTORY": str(self.root / "cache"), "BENCH_CACHE_REGISTRY": "", "GH_TOKEN": ""}
        missing = self.execute("ci-qemu-cache.sh", "pull", "guest", "aarch64", DIGEST, str(target), **environment)
        self.assertEqual(missing.returncode, 3)
        written = self.execute("ci-qemu-cache.sh", "push", "guest", "aarch64", DIGEST, str(source), "guest.qcow2", **environment)
        self.assertEqual(written.returncode, 0, written.stderr)
        restored = self.execute("ci-qemu-cache.sh", "pull", "guest", "aarch64", DIGEST, str(target), **environment)
        self.assertEqual(restored.returncode, 0, restored.stderr)
        self.assertEqual((source / "guest.qcow2").read_bytes(), (target / "guest.qcow2").read_bytes())

    def test_local_preparation_is_never_allowed_in_ci(self):
        result = subprocess.run(["bash", str(ROOT / "scripts/ci-qemu-benchmark.sh"),
                                 "prepare-local", "arm64", "hvf", "single"],
                                env=dict(os.environ, GITHUB_ACTIONS="true"), capture_output=True,
                                text=True, check=False, timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("prepare-local is forbidden", result.stderr)
        planned = subprocess.check_output(["bash", str(ROOT / "scripts/ci-qemu-benchmark.sh"),
                                           "plan", "arm64", "hvf", "single"], text=True)
        self.assertIn("-accel hvf", planned)
        self.assertIn("-cpu host", planned)

    def test_fresh_seeds_supply_network_configuration_for_cloned_guests(self):
        driver = (ROOT / "scripts/ci-qemu-benchmark.sh").read_text()
        self.assertIn('cp "$repo/scripts/qemu-guest-network.yaml" "$state/seed/network-config"', driver)
        self.assertIn('cloud-localds --network-config="$state/seed/network-config"', driver)
        self.assertIn("net=10.0.2.0/24", driver)
        network = (ROOT / "scripts/qemu-guest-network.yaml").read_text()
        self.assertIn("addresses: [10.0.2.15/24]", network)
        self.assertIn("dhcp4: false", network)
        self.assertIn("accept-ra: false", network)
        self.assertIn("link-local: []", network)
        self.assertIn("ipv6=off", driver)
        self.assertIn("52:54:00:12:34:56", network)

    def test_collection_retries_transport_without_reexecuting_tests(self):
        driver = (ROOT / "scripts/ci-qemu-benchmark.sh").read_text()
        collection = driver[driver.index("collected=false"):driver.index('if [ -s "$state/guest-results.tar.gz" ]')]
        for first_status, expected_calls, expected_result in ((255, 2, 23), (1, 1, 1), (0, 1, 23)):
            program = '''set -euo pipefail
state="$1"; result=23; calls=0; ssh_args=()
ssh() {
    calls=$((calls + 1))
    if [ "$calls" = 1 ]; then return "$2"; fi
    echo evidence
}
sleep() { :; }
''' + collection.replace('ssh "${ssh_args[@]}"', f'ssh ignored {first_status}') + '\nprintf "%s %s\\n" "$calls" "$result"'
            result = subprocess.run(["bash", "-c", program, "--", str(self.root)],
                                    capture_output=True, text=True, check=False, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), f"{expected_calls} {expected_result}")

    def test_image_cleanup_leaves_an_empty_machine_id_inode(self):
        script = (ROOT / "scripts/ci-qemu-image-clean.sh").read_text()
        self.assertLess(script.index("cloud-init clean"), script.index(": >/etc/machine-id"))
        self.assertLess(script.index(": >/etc/machine-id"), script.index("systemctl poweroff"))
        self.assertIn('test "${FLTH_GUEST:-}" = 1', script)
        self.assertIn("/etc/ssh/ssh_host_*", script)
        self.assertIn("fstrim -v /", script)
        self.assertIn('"scripts/ci-qemu-image-clean.sh"', (ROOT / "scripts/ci-qemu-artifacts.py").read_text())

    def test_compiler_does_not_inherit_registry_credentials(self):
        script = (ROOT / "scripts/ci-qemu-build.sh").read_text()
        self.assertLess(script.index("unset GH_TOKEN GITHUB_TOKEN"), script.index("cargo test --locked"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
