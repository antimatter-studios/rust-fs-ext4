#!/usr/bin/env bash
set -euo pipefail

fixture_targets() {
    case "$1" in
        single|smoke) echo basic ;;
        full) echo 'basic htree csum_seed no_csum deep_extents inline xattr acl largedir manyfiles whole_disk' ;;
        *) echo "unsupported workload: $1" >&2; return 2 ;;
    esac
}
if [ "${1:-}" = plan ]; then fixture_targets "${2:-}"; exit; fi

arch="${1:?guest architecture required}"
workload="${2:?workload required}"
[ "${FLTH_GUEST:-}" = 1 ] && [ "$(uname -m)" = "$arch" ] || { echo 'wrong guest or architecture' >&2; exit 1; }
cd -P /repo
mkdir -p tmp/qemu-results
evidence=/repo/tmp/qemu-results
exec > >(tee "$evidence/guest.log") 2>&1
uname -a >"$evidence/guest-uname.txt"
export DEBIAN_FRONTEND=noninteractive
export RUSTUP_HOME=/var/lib/fs-ext4-rust/rustup CARGO_HOME=/var/lib/fs-ext4-rust/cargo
export CARGO_TARGET_DIR=/benchmark-target
export PATH="$CARGO_HOME/bin:$PATH"
phase=setup
phase_started="$(date +%s)"
finish() {
    status=$?
    trap - EXIT
    printf '%s\t%s\t%s\n' "$phase" "$(( $(date +%s) - phase_started ))" "$status" >>"$evidence/phases.tsv"
    exit "$status"
}
trap finish EXIT
printf '[%s] guest=%s workload=%s phase=setup\n' "$(date -u +%FT%TZ)" "$arch" "$workload"
prepared=/var/lib/ext4-qemu-prepared.json
if [ -f "$prepared" ]; then
    python3 - "$prepared" "${BENCH_GUEST_KEY:?guest preparation key required}" <<'PY'
import json, sys
assert json.load(open(sys.argv[1]))["key"] == sys.argv[2], "prepared guest environment key mismatch"
PY
    printf '[%s] prepared guest cache hit; reusing installed tools and kernel-made fixtures\n' "$(date -u +%FT%TZ)"
    cp -a --sparse=always /var/cache/ext4-qemu-fixtures/. /repo/test-disks/
else
    [ "${BENCH_OPERATION:-run}" = prepare ] || {
        echo 'prepared guest marker missing; refusing cold setup in a measured trial' >&2
        exit 1
    }
    apt-get update
    apt-get install -y time python3 binutils ripgrep ca-certificates
    if [ "$workload" = single ]; then
        /usr/bin/time -v -o "$evidence/setup.time" apt-get install -y e2fsprogs util-linux
    else
        /usr/bin/time -v -o "$evidence/setup.time" bash scripts/vm-setup.sh
    fi
    if [ "$workload" = full ]; then
        printf '[%s] installing pinned cargo for metadata assertions, not compilation\n' "$(date -u +%FT%TZ)"
        /usr/bin/time -v -o "$evidence/toolchain.time" bash ../rust-fs-core/scripts/guest-rust-toolchain.sh
    fi
    printf '%s\t%s\t0\n' "$phase" "$(( $(date +%s) - phase_started ))" >>"$evidence/phases.tsv"
    phase=fixtures
    phase_started="$(date +%s)"
    printf '[%s] phase=%s\n' "$(date -u +%FT%TZ)" "$phase"
    read -r -a images <<<"$(fixture_targets "$workload")"
    /usr/bin/time -v -o "$evidence/fixtures.time" bash test-disks/guest-build-images.sh /repo/test-disks "${images[@]}"
    mkdir -p /var/cache/ext4-qemu-fixtures
    find /repo/test-disks -maxdepth 1 -type f \( -name '*.img' -o -name '*.meta.txt' \) \
        -exec cp --sparse=always -t /var/cache/ext4-qemu-fixtures -- {} +
    python3 - "$prepared" "${BENCH_GUEST_KEY:?guest preparation key required}" <<'PY'
import json, pathlib, sys
pathlib.Path(sys.argv[1]).write_text(json.dumps({"key": sys.argv[2]}) + "\n")
PY
fi
dpkg-query -W >"$evidence/guest-packages.txt"
printf '%s\t%s\t0\n' "$phase" "$(( $(date +%s) - phase_started ))" >>"$evidence/phases.tsv"
python3 scripts/ci-qemu-artifacts.py unpack /repo/tmp/test-bundle/test-bundle.tar.gz
FS_EXT4_TEST_TMPDIR="$(bash scripts/test.sh --print-temp-dir)"
export FS_EXT4_TEST_TMPDIR
export TMPDIR="$FS_EXT4_TEST_TMPDIR"
mkdir -p "$TMPDIR"
phase=tests
phase_started="$(date +%s)"
printf '[%s] phase=%s running prebuilt %s executables; no guest compilation\n' "$(date -u +%FT%TZ)" "$phase" "$arch"
set +e
/usr/bin/time -v -o "$evidence/tests.time" python3 scripts/ci-qemu-artifacts.py run \
    /repo/tmp/test-bundle/test-bundle.json "$arch" "$workload" 2>&1 | tee "$evidence/tests.log"
status=$?
set -e
if [ "$status" -ne 0 ]; then tail -60 "$evidence/tests.log"; exit "$status"; fi
python3 - "$evidence/tests.log" "$(python3 scripts/ci-qemu-artifacts.py floor "$workload")" "$evidence/passing-tests.txt" <<'PY'
import pathlib, re, sys
text = pathlib.Path(sys.argv[1]).read_text()
summaries = re.findall(r"test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", text)
assert summaries, "no executed test results"
assert all(int(failed) == 0 and int(ignored) == 0 for _, failed, ignored in summaries), "failed or ignored tests"
passed = sum(int(passed) for passed, _, _ in summaries)
assert passed >= int(sys.argv[2]), (passed, "below test floor", sys.argv[2])
if int(sys.argv[2]) == 1:
    assert passed == 1, (passed, "single-test trial must execute exactly one test")
names = sorted(re.findall(r"^test (.+) \.\.\. ok$", text, re.M))
assert len(names) == passed, "passing-test evidence is incomplete"
pathlib.Path(sys.argv[3]).write_text("\n".join(names) + "\n")
print(f"benchmark: {passed} tests passed; zero failed or ignored")
PY
