#!/usr/bin/env python3
import contextlib
import importlib.util
import io
import json
import http.server
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
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
        for name, arch in (("vps-x64", "x86_64"), ("vps-arm64", "aarch64")):
            trial = self.root / name
            trial.mkdir()
            (trial / "result.json").write_text(json.dumps({
                "guest_arch": arch, "accelerator": "tcg",
                "workload": "smoke", "guest_memory_mib": 2048, "guest_cpus": 2,
                "build_mode": "host-prebuilt", "status": 0, "elapsed_seconds": 1800,
                "finished_unix": 3500, "job_arrival_unix": 1705,
                "runner_environment": "self-hosted",
                "selected_runner": "antimatter-vps-linux-x64",
            }))
            (trial / "host.json").write_text(json.dumps({
                "job": name, "source_commit": "source", "core_commit": "core",
                "source_dirty": False,
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

    def check_timing(self, field, value, message):
        result_path = self.root / "vps-x64" / "result.json"
        result = json.loads(result_path.read_text())
        result[field] = value
        result_path.write_text(json.dumps(result))
        self.check_report(1, message=message)

    def test_negative_trial_duration_is_rejected(self):
        self.check_timing("elapsed_seconds", -1, "invalid trial duration")

    def test_boolean_trial_duration_is_rejected(self):
        self.check_timing("elapsed_seconds", False, "invalid trial duration")

    def test_trial_longer_than_allocated_job_is_rejected(self):
        self.check_timing("elapsed_seconds", 1801, "invalid trial duration")

    def test_finish_before_allocation_is_rejected(self):
        self.check_timing("finished_unix", 1699, "invalid measurement timestamps")

    def test_allocation_before_workflow_submission_is_rejected(self):
        self.jobs["jobs"][0]["steps"][0]["started_at"] = "1970-01-01T00:16:39Z"
        self.check_report(1, message="invalid measurement timestamps")

    def test_without_steps_uses_job_arrival_time(self):
        self.jobs["jobs"][0]["steps"] = []
        result_path = self.root / "vps-x64" / "result.json"
        result = json.loads(result_path.read_text())
        result["job_arrival_unix"] = 1700
        result_path.write_text(json.dumps(result))
        self.check_report(0)

    def check_vps_placement(self, field, value):
        for name in ("vps-x64", "vps-arm64"):
            with self.subTest(trial=name):
                result_path = self.root / name / "result.json"
                original = result_path.read_text()
                result = json.loads(original)
                if value is None:
                    del result[field]
                else:
                    result[field] = value
                result_path.write_text(json.dumps(result))
                try:
                    self.check_report(1, message=f"{name}: fell back to hosted; this is NOT a VPS measurement")
                finally:
                    result_path.write_text(original)

    def test_vps_trial_on_hosted_runner_is_rejected(self):
        self.check_vps_placement("selected_runner", "ubuntu-24.04")

    def test_vps_trial_on_hosted_environment_is_rejected(self):
        self.check_vps_placement("runner_environment", "github-hosted")

    def test_vps_trial_without_runner_label_is_rejected(self):
        self.check_vps_placement("selected_runner", None)

    def test_vps_trial_without_runner_environment_is_rejected(self):
        self.check_vps_placement("runner_environment", None)

    def test_hosted_named_trial_is_rejected(self):
        # No hosted trial runs, so a result claiming one is not a measurement.
        trial = self.root / "hosted-x64"
        trial.mkdir()
        for filename in (self.root / "vps-x64").iterdir():
            (trial / filename.name).write_bytes(filename.read_bytes())
        identity = json.loads((trial / "host.json").read_text())
        identity["job"] = "hosted-x64"
        (trial / "host.json").write_text(json.dumps(identity))
        self.check_report(1, message="hosted-x64: unexpected trial identity")

    def test_report_claims_no_hosted_comparison(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            qemu_report.report(self.root, self.jobs, self.run, "smoke", "source")
        self.assertIn("no hosted placement was measured or compared", output.getvalue())

    def test_missing_trial_description_is_rejected(self):
        result_path = self.root / "vps-arm64" / "result.json"
        result = json.loads(result_path.read_text())
        del result["guest_cpus"]
        result_path.write_text(json.dumps(result))
        self.check_report(1, message="vps-arm64: missing guest_cpus")

    def source_cleanliness(self, value):
        identity_path = self.root / "vps-x64" / "host.json"
        identity = json.loads(identity_path.read_text())
        identity["source_dirty"] = value
        identity_path.write_text(json.dumps(identity))

    def test_missing_source_cleanliness_is_rejected(self):
        identity_path = self.root / "vps-x64" / "host.json"
        identity = json.loads(identity_path.read_text())
        del identity["source_dirty"]
        identity_path.write_text(json.dumps(identity))
        self.check_report(1, message="source cleanliness is not verified")

    def test_null_source_cleanliness_is_rejected(self):
        self.source_cleanliness(None)
        self.check_report(1, message="source cleanliness is not verified")

    def test_numeric_source_cleanliness_is_rejected(self):
        self.source_cleanliness(0)
        self.check_report(1, message="source cleanliness is not verified")

    def test_empty_source_cleanliness_is_rejected(self):
        self.source_cleanliness("")
        self.check_report(1, message="source cleanliness is not verified")

    def test_dirty_source_is_rejected(self):
        self.source_cleanliness(True)
        self.check_report(1, message="source checkout is dirty")

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


class ReportInputFetch(unittest.TestCase):
    """The report job's API inputs arrive without `gh`, which the VPS lacks."""

    FETCH = pathlib.Path(__file__).with_name("ci-qemu-run-api.py")
    WORKFLOW = pathlib.Path(__file__).resolve().parent.parent / ".github/workflows/qemu-benchmark.yml"

    def setUp(self):
        self.requests = []
        self.status = 200
        self.body = b'{"jobs": []}'
        test = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                test.requests.append((self.path, dict(self.headers)))
                self.send_response(test.status)
                self.end_headers()
                self.wfile.write(test.body)

            def log_message(self, *args):
                pass

        self.server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.output = pathlib.Path(self.scratch.name) / "out.json"

    def fetch(self, path, token="secret-token"):
        # A PATH holding only this Python: no gh can be found.
        bin_dir = pathlib.Path(self.scratch.name) / "bin"
        bin_dir.mkdir(exist_ok=True)
        environment = {"PATH": str(bin_dir), "GH_TOKEN": token,
                       "GITHUB_API_URL": f"http://127.0.0.1:{self.server.server_port}"}
        return subprocess.run([sys.executable, str(self.FETCH), path, str(self.output)],
                              env=environment, capture_output=True, text=True)

    def test_fetch_needs_no_gh(self):
        path = "repos/o/r/actions/runs/7/attempts/2/jobs?per_page=100"
        result = self.fetch(path)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.output.read_bytes(), self.body)
        self.assertEqual(self.requests[0][0], "/" + path)
        self.assertEqual(self.requests[0][1]["Authorization"], "Bearer secret-token")

    def test_http_failure_fails_and_writes_nothing(self):
        self.status = 403
        result = self.fetch("repos/o/r/actions/runs/7")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("HTTP 403", result.stderr)
        self.assertFalse(self.output.exists())

    def test_non_json_body_fails(self):
        self.body = b"<html>"
        self.assertNotEqual(self.fetch("repos/o/r/actions/runs/7").returncode, 0)
        self.assertFalse(self.output.exists())

    def test_missing_token_fails(self):
        result = self.fetch("repos/o/r/actions/runs/7", token="")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.requests, [])

    def test_workflow_fetches_both_inputs_without_gh(self):
        step = self.WORKFLOW.read_text().split("  report:\n", 1)[1]
        self.assertNotRegex(step, r"(?m)^\s+gh ")
        self.assertIn('python3 scripts/ci-qemu-run-api.py "repos/$GITHUB_REPOSITORY/actions/runs/'
                      '$GITHUB_RUN_ID/attempts/$GITHUB_RUN_ATTEMPT/jobs?per_page=100" '
                      "tmp/benchmark-jobs.json", step)
        self.assertIn('python3 scripts/ci-qemu-run-api.py "repos/$GITHUB_REPOSITORY/actions/runs/'
                      '$GITHUB_RUN_ID" tmp/benchmark-run.json', step)
        self.assertIn("GH_TOKEN: ${{ github.token }}", step)


if __name__ == "__main__":
    loader = unittest.defaultTestLoader
    suite = unittest.TestSuite([loader.loadTestsFromTestCase(ExecutedTestEvidence),
                                loader.loadTestsFromTestCase(ReportInputFetch)])
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    sys.exit(not result.wasSuccessful())
