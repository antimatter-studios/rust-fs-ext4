#!/usr/bin/env python3
import datetime
import json
import pathlib
import re
import subprocess
import sys


def timestamp(value):
    return int(datetime.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp())


def read_text(root, name):
    return (root / name).read_text().strip()


def executed_tests(root, workload, floor):
    log = root / "tests.log"
    if not log.exists():
        return 0, 0, 0, ["missing tests.log; cannot verify executed tests"]
    text = log.read_text()
    summaries = re.findall(
        r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; "
        r"\d+ measured; (\d+) filtered out;", text, re.M)
    if not summaries:
        return 0, 0, 0, ["no executed test results"]
    passed = sum(int(summary[1]) for summary in summaries)
    failed = sum(int(summary[2]) for summary in summaries)
    ignored = sum(int(summary[3]) for summary in summaries)
    problems = []
    if failed or ignored or any(summary[0] != "ok" for summary in summaries):
        problems.append("failed or ignored tests")
    if passed < floor:
        problems.append(f"{passed} executed tests below test floor {floor}")
    if workload == "single" and passed != 1:
        problems.append("single workload must execute exactly one test")
    if workload == "full" and any(int(summary[4]) for summary in summaries):
        problems.append("filtered full-workload tests")
    names = sorted(re.findall(r"^test (.+) \.\.\. ok$", text, re.M))
    if len(names) != passed:
        problems.append("passing-name count does not match executed summaries")
    names_path = root / "passing-tests.txt"
    if not names_path.exists():
        problems.append("missing passing-tests.txt")
    elif sorted(names_path.read_text().splitlines()) != names:
        problems.append("passing-name evidence does not match tests.log")
    return passed, failed, ignored, problems


def report(root, jobs, run, workload, source_commit):
    expected = {"hosted-x64": ("x86_64", "kvm"), "vps-x64": ("x86_64", "tcg"),
                "hosted-arm64": ("aarch64", "tcg"), "vps-arm64": ("aarch64", "tcg")}
    by_name = {job["name"]: job for job in jobs["jobs"]}
    created = timestamp(run["created_at"])
    measurements = {}
    problems = []
    floor = int(subprocess.check_output([
        sys.executable, str(pathlib.Path(__file__).with_name("ci-qemu-artifacts.py")),
        "floor", workload], text=True))
    if not source_commit:
        problems.append("missing workflow source commit; cannot verify artifact identity")
    print("## QEMU pilot: time to a verified result")
    print(f"Requested source: `{source_commit}`; workload: `{workload}`.")
    print("Queue wait is measured from workflow submission to the first job step.")
    print("Total time includes the job queue, setup, and workload; no hosted bootstrap gates the trials.")
    print()
    print("| Trial | Actual placement | Guest / accelerator | Queue (s) | Trial (s) | Total (s) | Result | Passed | Failed | Ignored |")
    print("|---|---|---|---:|---:|---:|---|---:|---:|---:|")
    for path in sorted(root.rglob("result.json")):
        result = json.loads(path.read_text())
        host_path = path.with_name("host.json")
        if not host_path.exists():
            problems.append(f"{path.parent.name}: no host identity; trial failed before workload")
            continue
        host = json.loads(host_path.read_text())
        name = host["job"]
        if name not in expected:
            problems.append(f"{name}: unexpected trial identity")
            continue
        if host.get("source_dirty") is not False:
            problems.append(f"{name}: source checkout is dirty or source cleanliness is not verified; "
                            "commit identity does not describe the workload")
        if source_commit and host.get("source_commit") != source_commit:
            problems.append(f"{name}: artifact source does not match workflow source {source_commit}")
        if result.get("workload") != workload:
            problems.append(f"{name}: artifact workload does not match requested workload {workload}")
        if (result.get("guest_arch"), result.get("accelerator")) != expected[name]:
            arch, accelerator = expected[name]
            problems.append(f"{name}: expected {arch} / {accelerator}")
        if name in measurements:
            problems.append(f"{name}: duplicate result")
            continue
        measurements[name] = (result, path.parent, host)
        steps = by_name.get(name, {}).get("steps", [])
        allocation = timestamp(steps[0]["started_at"]) if steps else result["job_arrival_unix"]
        queue = max(0, allocation - created)
        total = result["finished_unix"] - created
        placement = result["selected_runner"]
        verdict = "passed" if result["status"] == 0 else f"failed ({result['status']})"
        passed, failed, ignored, test_problems = executed_tests(path.parent, workload, floor)
        problems.extend(f"{name}: {problem}" for problem in test_problems)
        print(f"| {name} | {placement} | {result['guest_arch']} / {result['accelerator']} | "
              f"{queue} | {result['elapsed_seconds']} | {total} | {verdict} | {passed} | {failed} | {ignored} |")
        if result["status"] != 0:
            problems.append(f"{name}: workload failed; inspect guest, console and time logs")
        if name.startswith("vps-") and (
            placement != "antimatter-vps-linux-x64" or result["runner_environment"] != "self-hosted"
        ):
            problems.append(f"{name}: fell back to hosted; this is NOT a VPS measurement")
        if by_name.get(name, {}).get("conclusion") != "success":
            problems.append(f"{name}: job did not complete successfully")
    for missing in sorted(expected.keys() - measurements.keys()):
        job = by_name.get(missing, {})
        problems.append(f"{missing}: no measurements (job {job.get('conclusion') or job.get('status', 'missing')})")
    for suffix in ("x64", "arm64"):
        names = [f"hosted-{suffix}", f"vps-{suffix}"]
        if not all(name in measurements for name in names):
            continue
        left, right = [measurements[name] for name in names]
        for field in ("guest_arch", "workload", "guest_memory_mib", "guest_cpus", "build_mode"):
            if field not in left[0] or field not in right[0]:
                problems.append(f"{suffix}: missing {field}; cannot compare")
            elif left[0][field] != right[0][field]:
                problems.append(f"{suffix}: mismatched {field}; cannot compare")
        for field in ("source_commit", "core_commit"):
            if left[2][field] != right[2][field]:
                problems.append(f"{suffix}: mismatched {field}; cannot compare")
        for filename in ("passing-tests.txt", "rustc.txt", "builder-image.txt", "build-packages.txt",
                         "test-cache-key.txt", "guest-packages.txt"):
            if not all((trial[1] / filename).exists() for trial in (left, right)):
                problems.append(f"{suffix}: missing {filename}")
            elif read_text(left[1], filename) != read_text(right[1], filename):
                problems.append(f"{suffix}: {filename} differs; workloads are not equivalent")
    print()
    print("Native compile provenance: `compile.time`; actual build/restore time: `host-build-seconds.txt`.")
    print("Guest test max RSS and wall time: `tests.time`; QEMU RSS: `result.json`; reuse: `cache-events.tsv`.")
    print("One smoke result is not evidence that the full pipeline is safe to migrate.")
    if problems:
        print("\n### Incomplete or invalid comparison")
        for problem in problems:
            print(f"- {problem}")
        return 1
    print("\nBoth placements completed matching tests. Compare total time, not runtime alone; repeat before rollout.")
    return 0


if __name__ == "__main__":
    sys.exit(report(pathlib.Path(sys.argv[1]), json.loads(pathlib.Path(sys.argv[2]).read_text()),
                    json.loads(pathlib.Path(sys.argv[3]).read_text()), sys.argv[4], sys.argv[5]))
