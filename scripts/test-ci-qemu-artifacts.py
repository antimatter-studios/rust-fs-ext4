#!/usr/bin/env python3
import hashlib
import importlib.util
import io
import os
import pathlib
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.dont_write_bytecode = True


class HostBuildContract(unittest.TestCase):
    def test_runtime_build_reuses_a_provisioned_runner(self):
        build = (ROOT / "scripts/ci-qemu-build.sh").read_text()
        self.assertNotIn("apt-get", build)
        self.assertNotIn("rustup target add", build)
        self.assertNotIn("bash /rust-fs-core/scripts/guest-rust-toolchain.sh", build)
        action = (ROOT / ".github/actions/qemu-benchmark/action.yml").read_text()
        self.assertNotIn("apt-get", action)
        self.assertIn("ci-qemu-runner.sh", action)

    def test_prepared_guest_policy_precedes_compilation(self):
        driver = (ROOT / "scripts/ci-qemu-benchmark.sh").read_text()
        self.assertLess(driver.index(' policy "$action"'),
                        driver.index('bash "$repo/scripts/ci-qemu-build.sh"'))

    def test_guest_does_not_compile(self):
        guest = (ROOT / "scripts/ci-qemu-guest.sh").read_text()
        self.assertNotIn("phase=compile", guest)
        self.assertNotIn("--no-run", guest)
        self.assertIn("ci-qemu-artifacts.py", guest)
        self.assertLess(guest.index('${BENCH_OPERATION:-run}'), guest.index("apt-get update"))

    def test_host_build_precedes_guest_boot(self):
        driver = (ROOT / "scripts/ci-qemu-benchmark.sh").read_text()
        self.assertLess(driver.index('bash "$repo/scripts/ci-qemu-build.sh"'),
                        driver.index('"${command_args[@]}" >"$state/qemu.log"'))
        build = (ROOT / "scripts/ci-qemu-build.sh").read_text()
        self.assertIn("--target", build)
        self.assertNotIn("--memory", build)
        self.assertNotIn("--cpus", build)
        self.assertNotIn("BENCH_CPUS", build)
        self.assertIn("debian:12-slim@sha256:", build)
        self.assertIn('test "$(uname -m)" = "$BUILD_HOST_ARCH"', build)


class ArtifactTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location(
            "qemu_artifacts", ROOT / "scripts/ci-qemu-artifacts.py")
        cls.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.module)

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = pathlib.Path(self.temporary.name)
        self.target = self.root / "target"
        self.target.mkdir()

    def test_local_preparation_accepts_native_aliases_and_explicit_tcg(self):
        self.assertEqual(self.module.normalize_arch("arm64"), "aarch64")
        self.assertEqual(self.module.normalize_arch("amd64"), "x86_64")
        self.module.guest_policy("prepare-local", "aarch64", "hvf", "arm64", False)
        self.module.guest_policy("prepare-local", "x86_64", "tcg", "arm64", False)
        with self.assertRaises(ValueError):
            self.module.guest_policy("prepare-local", "x86_64", "hvf", "arm64", False)

    def test_runner_environment_key_tracks_setup_not_source(self):
        repository, core = self.root / "repo", self.root / "rust-fs-core"
        for name in ("scripts/qemu-runner/Dockerfile", "scripts/qemu-runner/setup.sh",
                     "scripts/qemu-runner/packages.txt", "rust-toolchain.toml", "src/lib.rs"):
            target = repository / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text("original")
        installer = core / "scripts/guest-rust-toolchain.sh"
        installer.parent.mkdir(parents=True)
        installer.write_text("original")
        original = self.module.runner_recipe(repository, core, "arm64")
        self.assertEqual(original, self.module.runner_recipe(repository, core, "aarch64"))
        (repository / "src/lib.rs").write_text("changed source")
        with mock.patch.dict(os.environ, ACCELERATOR="tcg", GITHUB_JOB="other"):
            self.assertEqual(original, self.module.runner_recipe(repository, core, "aarch64"))
        (repository / "scripts/qemu-runner/packages.txt").write_text("changed packages")
        self.assertNotEqual(original["key"], self.module.runner_recipe(repository, core, "aarch64")["key"])
        self.assertNotEqual(original["key"], self.module.runner_recipe(repository, core, "x86_64")["key"])

    def executable(self, name, machine=62):
        executable = self.target / name
        header = bytearray(64)
        header[:6] = b"\x7fELF\x02\x01"
        struct.pack_into("<H", header, 18, machine)
        executable.write_bytes(header)
        executable.chmod(0o755)
        return executable

    def record(self, executable, test=True):
        return {"reason": "compiler-artifact", "executable": str(executable),
                "profile": {"test": test}, "target": {"name": executable.name}}

    def test_workload_scope_and_filters(self):
        single = self.module.workload_plan("single")
        self.assertEqual(single["targets"], ["--test", "capi_basic"])
        self.assertEqual(single["filter"], ["mount_and_umount_basic_image", "--exact", "--nocapture"])
        self.assertEqual(single["floor"], 1)
        self.assertEqual(self.module.workload_plan("smoke")["filter"], ["--skip", "needs_host::"])
        self.assertEqual(self.module.workload_plan("full")["profile"], ["--profile", "test-opt"])
        with self.assertRaises(ValueError):
            self.module.workload_plan("unknown")

    def test_cold_guests_require_explicit_native_preparation(self):
        policy = self.module.guest_policy
        for arch in ("x86_64", "aarch64"):
            with self.subTest(arch=arch), self.assertRaisesRegex(ValueError, "prepared guest"):
                policy("run", arch, "tcg", "x86_64", False)
            policy("run", arch, "tcg", "x86_64", True)
        with self.assertRaisesRegex(ValueError, "native"):
            policy("prepare", "aarch64", "tcg", "x86_64", False)
        with self.assertRaisesRegex(ValueError, "KVM"):
            policy("prepare", "x86_64", "tcg", "x86_64", False)
        policy("prepare", "x86_64", "kvm", "x86_64", False)
        policy("prepare", "aarch64", "tcg", "aarch64", False)

    def test_collects_tests_and_cli_without_running_either(self):
        records = [self.record(self.executable("test-bin")),
                   self.record(self.executable("cli"), test=False)]
        with mock.patch.object(self.module.subprocess, "run", side_effect=AssertionError("host ran target")):
            artifacts = self.module.collect(records, self.target, "x86_64")
        self.assertEqual([artifact["test"] for artifact in artifacts], [True, False])
        self.assertEqual(artifacts[0]["sha256"], hashlib.sha256(
            pathlib.Path(artifacts[0]["path"]).read_bytes()).hexdigest())

    def test_wrong_architecture_missing_tests_and_escaping_paths_fail(self):
        arm = self.record(self.executable("arm", machine=183))
        with self.assertRaisesRegex(ValueError, "ELF machine"):
            self.module.collect([arm], self.target, "x86_64")
        self.assertEqual(len(self.module.collect([arm], self.target, "aarch64")), 1)
        with self.assertRaisesRegex(ValueError, "no test executables"):
            self.module.collect([dict(arm, profile={"test": False})], self.target, "aarch64")
        with self.assertRaises(ValueError):
            self.module.collect([arm], self.root / "other", "aarch64")

    def test_runner_rejects_corruption_and_workload_mismatch(self):
        executable = self.executable("test-bin")
        artifacts = self.module.collect([self.record(executable)], self.target, "x86_64")
        manifest = {"arch": "x86_64", "workload": "single", "artifacts": artifacts}
        self.module.verify(manifest, "x86_64", "single")
        with self.assertRaises(ValueError):
            self.module.verify(manifest, "aarch64", "single")
        with self.assertRaises(ValueError):
            self.module.verify(manifest, "x86_64", "full")
        executable.write_bytes(executable.read_bytes() + b"corrupt")
        with self.assertRaisesRegex(ValueError, "checksum"):
            self.module.verify(manifest, "x86_64", "single")

    def test_runner_executes_matching_test_with_exact_filter(self):
        executable = self.executable("test-bin")
        artifacts = self.module.collect([self.record(executable)], self.target, "x86_64")
        manifest = {"arch": "x86_64", "workload": "single", "artifacts": artifacts}
        with mock.patch.object(self.module.subprocess, "run") as run:
            run.return_value.returncode = 0
            self.assertEqual(self.module.run_tests(manifest, "x86_64", "single"), 0)
            run.assert_called_once_with([str(executable), "mount_and_umount_basic_image",
                                         "--exact", "--nocapture"], check=False)
            run.return_value.returncode = 17
            self.assertEqual(self.module.run_tests(manifest, "x86_64", "single"), 17)

    def test_unpack_rejects_traversal_and_links(self):
        for name, kind in (("../escaped", tarfile.REGTYPE),
                           ("benchmark-target/link", tarfile.SYMTYPE)):
            bundle = self.root / "unsafe.tar.gz"
            with tarfile.open(bundle, "w:gz") as archive:
                member = tarfile.TarInfo(name)
                member.type = kind
                member.linkname = "/etc/passwd" if kind == tarfile.SYMTYPE else ""
                archive.addfile(member)
            with self.assertRaisesRegex(ValueError, "unsafe"):
                self.module.unpack(bundle, self.root / "unpacked")

    def test_unpack_preserves_executable_bytes(self):
        bundle = self.root / "valid.tar.gz"
        with tarfile.open(bundle, "w:gz") as archive:
            member = tarfile.TarInfo("benchmark-target/arch/debug/test-bin")
            member.size = 7
            archive.addfile(member, io.BytesIO(b"payload"))
        self.module.unpack(bundle, self.root / "unpacked")
        executable = self.root / "unpacked/benchmark-target/arch/debug/test-bin"
        self.assertEqual(executable.read_bytes(), b"payload")
        self.assertTrue(os.access(executable, os.X_OK))

    def test_cache_keys_separate_sources_from_environment(self):
        repository, core = self.root / "repo", self.root / "rust-fs-core"
        files = ["scripts/ci-qemu-guest.sh", "scripts/ci-qemu-boot.sh", "scripts/ci-qemu-image-clean.sh",
                 "scripts/qemu-guest-network.yaml", "scripts/vm-setup.sh",
                 "test-disks/guest-build-images.sh", "rust-toolchain.toml", "src/lib.rs"]
        for name in files:
            path = repository / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("original")
        (core / "scripts").mkdir(parents=True)
        (core / "scripts/guest-rust-toolchain.sh").write_text("pinned")

        def tracked(root):
            return [str(path.relative_to(root)) for path in root.rglob("*") if path.is_file()]

        with mock.patch.object(self.module, "tracked_inputs", side_effect=tracked):
            original = self.module.cache_keys(repository, core, "x86_64", "single")
            with mock.patch.dict(os.environ, GITHUB_JOB="different-runner", ACCELERATOR="tcg"):
                self.assertEqual(original, self.module.cache_keys(repository, core, "x86_64", "single"))
            (repository / "src/lib.rs").write_text("new source")
            changed = self.module.cache_keys(repository, core, "x86_64", "single")
            self.assertNotEqual(original["tests"], changed["tests"])
            self.assertEqual(original["guest"], changed["guest"])
            (repository / "scripts/vm-setup.sh").write_text("new package list")
            environment = self.module.cache_keys(repository, core, "x86_64", "single")
            self.assertNotEqual(changed["guest"], environment["guest"])
            self.assertNotEqual(changed["tests"], environment["tests"])
            arm = self.module.cache_keys(repository, core, "aarch64", "single")
            self.assertNotEqual(environment["tests"], arm["tests"])
            self.assertNotEqual(environment["guest"], arm["guest"])
            (repository / "rust-toolchain.toml").write_text("new compiler")
            compiler = self.module.cache_keys(repository, core, "x86_64", "single")
            self.assertNotEqual(environment["tests"], compiler["tests"])
            self.assertNotEqual(environment["guest"], compiler["guest"])


class CacheCommandTests(unittest.TestCase):
    def test_missing_prepared_image_fails_before_boot_or_compilation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            tools = root / "tools"
            tools.mkdir()
            repository = root / "repo"
            for name in ("scripts/ci-qemu-artifacts.py", "scripts/ci-qemu-cache.sh",
                         "scripts/qemu-pilot-environment.json", "scripts/ci-qemu-guest.sh",
                         "scripts/ci-qemu-boot.sh", "scripts/ci-qemu-image-clean.sh",
                         "scripts/qemu-guest-network.yaml", "scripts/vm-setup.sh",
                         "test-disks/guest-build-images.sh", "rust-toolchain.toml"):
                target = repository / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes((ROOT / name).read_bytes())
            core_setup = root / "rust-fs-core/scripts/guest-rust-toolchain.sh"
            core_setup.parent.mkdir(parents=True)
            core_setup.write_text("pinned fixture")
            git = tools / "git"
            git.write_text('#!/usr/bin/env bash\n'
                           'if [ "$1" = -C ]; then cd "$2"; shift 2; fi\n'
                           'case "$1" in\nrev-parse) echo fixture-revision ;;\n'
                           'status) exit 0 ;;\n'
                           'ls-files) find . -path ./tmp -prune -o -type f -print0 ;;\nesac\n')
            git.chmod(0o755)
            for name in ("qemu-system-x86_64", "qemu-img", "docker", "curl"):
                tool = tools / name
                tool.write_text('#!/usr/bin/env bash\n'
                                'if [ "${1:-}" = --version ]; then echo fake-qemu; exit 0; fi\n'
                                'echo unexpected-work >"$UNEXPECTED_WORK"\nexit 99\n')
                tool.chmod(0o755)
            environment = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"],
                               BENCH_CACHE_REGISTRY="", UNEXPECTED_WORK=str(root / "unexpected"),
                               QEMU_BENCHMARK_REPO=str(repository), QEMU_BENCHMARK_FROZEN="1")
            result = subprocess.run(["bash", str(ROOT / "scripts/ci-qemu-benchmark.sh"),
                                     "run", "x86_64", "tcg", "single"], env=environment,
                                    capture_output=True, text=True, check=False, timeout=15)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("prepared guest cache miss", result.stdout + result.stderr)
            self.assertFalse((root / "unexpected").exists())

    def test_registry_misses_are_not_auth_or_network_failures(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            tools = root / "tools"
            tools.mkdir()
            oras = tools / "oras"
            oras.write_text('#!/usr/bin/env bash\ncase "$1" in\n'
                            'login) cat >/dev/null; exit 0 ;;\n'
                            'pull) echo "$CACHE_ERROR" >&2; exit 1 ;;\n'
                            'esac\nexit 0\n')
            oras.chmod(0o755)
            environment = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"],
                               BENCH_CACHE_REGISTRY="ghcr.io/example/repo/qemu-pilot",
                               GH_TOKEN="not-a-real-token", GITHUB_ACTOR="example")
            command = ["bash", str(ROOT / "scripts/ci-qemu-cache.sh"), "pull", "tests", "aarch64",
                       "a" * 64, str(root / "output")]
            for message, expected in (("manifest unknown: not found", 3),
                                      ("unauthorized: permission denied", 1),
                                      ("network connection refused", 1)):
                result = subprocess.run(command, check=False, env=dict(environment, CACHE_ERROR=message),
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                self.assertNotIn("not-a-real-token", result.stdout + result.stderr)

    def test_cross_compile_uses_native_container_not_target_emulation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            tools = root / "tools"
            tools.mkdir()
            docker = tools / "docker"
            docker.write_text('#!/usr/bin/env bash\nif [ "$1" = info ]; then echo amd64; '
                              'else printf "%s\\n" "$@" >"$DOCKER_CALL"; fi\n')
            docker.chmod(0o755)
            base = {key: value for key, value in os.environ.items()
                    if key != "BENCH_RUNNER_ENVIRONMENT"}
            environment = dict(base, PATH=str(tools) + os.pathsep + os.environ["PATH"],
                               DOCKER_CALL=str(root / "docker-call"),
                               BENCH_RUNNER_IMAGE="sha256:" + "a" * 64)

            def build_arguments(**extra):
                subprocess.run(["bash", str(ROOT / "scripts/ci-qemu-build.sh"), "aarch64", "single", str(root)],
                               env=dict(environment, **extra), check=True)
                return (root / "docker-call").read_text().splitlines()

            # The build container is never limited by this script, on any runner.
            for runner in ("self-hosted", "github-hosted", ""):
                arguments = build_arguments(BENCH_RUNNER_ENVIRONMENT=runner)
                self.assertEqual(arguments[arguments.index("--platform") + 1], "linux/amd64")
                self.assertIn("BUILD_HOST_ARCH=x86_64", arguments)
                self.assertFalse([argument for argument in arguments
                                  if argument.startswith(("--cpus", "--memory"))])

if __name__ == "__main__":
    unittest.main(verbosity=2)
