#!/usr/bin/env python3
import contextlib
import importlib.util
import io
import json
import pathlib
import sys
import tempfile
import unittest


sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "qemu_report", pathlib.Path(__file__).with_name("report-qemu-benchmark.py"))
qemu_report = importlib.util.module_from_spec(spec)
spec.loader.exec_module(qemu_report)


class ExecutedTestEvidence(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = pathlib.Path(self.scratch.name)
        self.jobs = {"jobs": []}
        self.run = {"created_at": "1970-01-01T00:16:40Z"}
        for name, arch in (("hosted-x64", "x86_64"), ("vps-x64", "x86_64"),
                           ("hosted-arm64", "aarch64"), ("vps-arm64", "aarch64")):
            trial = self.root / name
            trial.mkdir()
            vps = name.startswith("vps")
            (trial / "result.json").write_text(json.dumps({
                "guest_arch": arch, "accelerator": "tcg" if vps or arch == "aarch64" else "kvm",
                "workload": "smoke", "guest_memory_mib": 2048, "guest_cpus": 2,
                "build_mode": "host-prebuilt", "status": 0, "elapsed_seconds": 1800,
                "finished_unix": 3500, "job_arrival_unix": 1705,
                "runner_environment": "self-hosted" if vps else "github-hosted",
                "selected_runner": "antimatter-vps-linux-x64" if vps else "ubuntu-24.04",
            }))
            (trial / "host.json").write_text(json.dumps({
                "job": name, "source_commit": "source", "core_commit": "core",
            }))
            for filename in ("rustc.txt", "builder-image.txt", "build-packages.txt",
                             "test-cache-key.txt", "guest-packages.txt"):
                (trial / filename).write_text("matching-input\n")
            self.jobs["jobs"].append({
                "name": name, "conclusion": "success",
                "steps": [{"started_at": "1970-01-01T00:28:20Z"}],
            })
        self.evidence()

    def evidence(self, workload="smoke", count=440, failed=0, ignored=0,
                 filtered=0, repeated=False):
        names = ["repeated" if repeated else f"case_{index}" for index in range(count)]
        verdict = "FAILED" if failed else "ok"
        log = "".join(f"test {name} ... ok\n" for name in names)
        log += (f"test result: {verdict}. {count} passed; {failed} failed; {ignored} ignored; "
                f"0 measured; {filtered} filtered out; finished in 1.00s\n")
        for trial in self.root.iterdir():
            result_path = trial / "result.json"
            result = json.loads(result_path.read_text())
            result["workload"] = workload
            result_path.write_text(json.dumps(result))
            (trial / "tests.log").write_text(log)
            (trial / "passing-tests.txt").write_text("\n".join(sorted(names)) + "\n")

    def check_report(self, expected, workload="smoke", message=None):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            result = qemu_report.report(self.root, self.jobs, self.run, workload, "source")
        self.assertEqual(result, expected, output.getvalue())
        if message:
            self.assertIn(message, output.getvalue())

    def test_matching_full_workload_passes(self):
        self.evidence(workload="full", count=1078)
        self.check_report(0, workload="full")

    def test_matching_smoke_workload_passes(self):
        self.check_report(0)

    def test_report_contract_changes_trigger_the_workflow(self):
        workflow = pathlib.Path(__file__).resolve().parent.parent / ".github/workflows/qemu-benchmark.yml"
        self.assertIn("      - scripts/test-report-qemu-benchmark.py\n", workflow.read_text())

    def test_single_executes_exactly_one(self):
        self.evidence(workload="single", count=1)
        self.check_report(0, workload="single")
        self.evidence(workload="single", count=2)
        self.check_report(1, workload="single", message="exactly one test")

    def test_missing_raw_log_is_rejected(self):
        for trial in self.root.iterdir():
            (trial / "tests.log").rename(trial / "tests.missing")
        self.check_report(1, message="missing tests.log")

    def test_empty_evidence_is_rejected(self):
        for trial in self.root.iterdir():
            (trial / "tests.log").write_text("")
            (trial / "passing-tests.txt").write_text("")
        self.check_report(1, message="no executed test results")

    def test_full_floor_is_preserved(self):
        self.evidence(workload="full", count=1077)
        self.check_report(1, workload="full", message="below test floor 1078")

    def test_smoke_floor_is_preserved(self):
        self.evidence(count=439)
        self.check_report(1, message="below test floor 440")

    def test_failed_tests_are_rejected(self):
        self.evidence(failed=1)
        self.check_report(1, message="failed or ignored tests")

    def test_ignored_tests_are_rejected(self):
        self.evidence(ignored=1)
        self.check_report(1, message="failed or ignored tests")

    def test_filtered_full_tests_are_rejected(self):
        self.evidence(workload="full", count=1078, filtered=1)
        self.check_report(1, workload="full", message="filtered full-workload tests")

    def test_missing_passing_name_is_rejected(self):
        for trial in self.root.iterdir():
            names_path = trial / "passing-tests.txt"
            names_path.write_text("\n".join(names_path.read_text().splitlines()[1:]) + "\n")
        self.check_report(1, message="passing-name evidence does not match tests.log")

    def test_replaced_passing_name_is_rejected(self):
        for trial in self.root.iterdir():
            names_path = trial / "passing-tests.txt"
            names_path.write_text(names_path.read_text().replace("case_0\n", "fabricated\n"))
        self.check_report(1, message="passing-name evidence does not match tests.log")

    def test_raw_name_count_matches_summaries(self):
        for trial in self.root.iterdir():
            log_path = trial / "tests.log"
            log_path.write_text(log_path.read_text().replace("test case_0 ... ok\n", ""))
        self.check_report(1, message="passing-name count does not match executed summaries")

    def test_missing_passing_names_are_rejected(self):
        for trial in self.root.iterdir():
            (trial / "passing-tests.txt").rename(trial / "passing-tests.missing")
        self.check_report(1, message="missing passing-tests.txt")

    def test_duplicate_name_occurrences_are_preserved(self):
        self.evidence(repeated=True)
        self.check_report(0)


if __name__ == "__main__":
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(ExecutedTestEvidence)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    sys.exit(not result.wasSuccessful())
