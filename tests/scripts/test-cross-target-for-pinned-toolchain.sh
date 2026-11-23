#!/usr/bin/env bash
# A job that builds for another target installs it for the pinned toolchain.
#
# rust-toolchain.toml pins the channel, and inside the checkout that pin
# overrides whatever `dtolnay/rust-toolchain@stable` set as the default.
# The action's `targets:` input installs the target for `stable` only, so
# a cross build then runs on the pinned toolchain with no standard library
# for the target. That is how the ARM64 lint on the x86_64 VPS runner
# failed in run 45176432202 (job 137722514466): `rustc` 1.95.0 from the pin
# was in use and stopped with "error[E0463]: can't find crate for `core`"
# for aarch64-unknown-linux-gnu, although the action had installed that
# target for stable 1.100.0.
#
# Refused in every workflow: a job with a step whose env sets
# CARGO_BUILD_TARGET, and no earlier step that runs
# `rustup target add <that target>` in the checkout (so the pin decides
# the toolchain the target is added to). Also refused: a `targets:` input
# on dtolnay/rust-toolchain, which never reaches the pinned toolchain.
#
#   bash tests/scripts/test-cross-target-for-pinned-toolchain.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

fail() { echo "FAIL  $*" >&2; exit 1; }

command -v python3 >/dev/null 2>&1 || fail "python3 is required; run 'chore tools'"
python3 -c 'import yaml' 2>/dev/null ||
    fail "the python3 yaml module is required (pip install pyyaml)"

grep -Eq '^channel *= *"[0-9]' "$REPO/rust-toolchain.toml" ||
    fail "rust-toolchain.toml no longer pins a numbered channel; revisit this test"

scan() {
    python3 - "$@" <<'PY'
import re, sys, yaml
bad = []
for path in sys.argv[1:]:
    with open(path) as f:
        doc = yaml.safe_load(f) or {}
    for job, spec in (doc.get("jobs") or {}).items():
        added = set()
        for i, step in enumerate(spec.get("steps") or []):
            uses = step.get("uses") or ""
            if uses.startswith("dtolnay/rust-toolchain") and (step.get("with") or {}).get("targets"):
                bad.append(f"{path}: {job}: step {i} passes targets: to {uses}, "
                           "which installs it for that toolchain, not the rust-toolchain.toml pin")
            run = step.get("run") or ""
            for m in re.finditer(r"rustup\s+target\s+add\s+([^\n;&|]+)", run):
                added.update(m.group(1).split())
            target = (step.get("env") or {}).get("CARGO_BUILD_TARGET")
            if target and target not in added:
                bad.append(f"{path}: {job}: step {i} ({step.get('name', '?')}) builds for "
                           f"{target} with no earlier `rustup target add {target}`")
for line in bad:
    print(line)
sys.exit(1 if bad else 0)
PY
}

# The scan must catch the shape that failed before it may pass the tree.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cat >"$tmp/broken.yml" <<'YML'
jobs:
  test-arm64:
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: aarch64-unknown-linux-gnu
      - name: chore lint, for the ARM64 target
        env:
          CARGO_BUILD_TARGET: aarch64-unknown-linux-gnu
        run: chore lint
YML
scan "$tmp/broken.yml" >/dev/null && fail "the scan passed the job that failed in run 45176432202"

out="$(scan "$REPO"/.github/workflows/*.yml)" || fail "$out"
echo "ok    every cross-target build adds its target to the pinned toolchain first"
