#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sandbox="$(mktemp -d)"
trap 'rm -r "$sandbox"' EXIT
mkdir -p "$sandbox/repo/scripts" "$sandbox/bin"
cp "$repo/scripts/test.sh" "$sandbox/repo/scripts/"
export GUEST_ROOT="$sandbox/guest"
export FS_EXT4_GUEST_SSH="$sandbox/ssh_config"
export PATH="$sandbox/bin:$PATH"
touch "$FS_EXT4_GUEST_SSH"
mkdir -p "$GUEST_ROOT$sandbox/repo/test-disks" "$GUEST_ROOT$sandbox/repo/tmp/logs"
printf 'fixture\n' >"$GUEST_ROOT$sandbox/repo/test-disks/shared.img"
printf 'evidence\n' >"$GUEST_ROOT$sandbox/repo/tmp/logs/oracle.log"

cat >"$sandbox/bin/cargo" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
mkdir -p "$GUEST_ROOT$FS_EXT4_TEST_TMPDIR"
printf '%s\n' "$FS_EXT4_TEST_TMPDIR" >"$GUEST_ROOT/path"
touch "$GUEST_ROOT$FS_EXT4_TEST_TMPDIR/fixture-copy.img"
exit "$TEST_STATUS"
STUB
cat >"$sandbox/bin/ssh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[[ "$#" == 5 && "$1" == -n && "$2" == -F && "$3" == "$FS_EXT4_GUEST_SSH" && "$4" == guest ]]
scratch="$(cat "$GUEST_ROOT/path")"
printf -v host '%q' "$scratch"
printf -v guest '%q' "$GUEST_ROOT$scratch"
exec bash -c "${5//"$host"/"$guest"}"
STUB
cat >"$sandbox/bin/fstrim" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[[ "$*" == / && ! -e "$GUEST_ROOT$(cat "$GUEST_ROOT/path")" ]]
printf 'trim\n' >>"$GUEST_ROOT/trims"
STUB
chmod +x "$sandbox/bin/"*

fail() { echo "FAIL  $1" >&2; exit 1; }
for expected in 0 17; do
    status=0
    env -u FS_EXT4_TEST_TMPDIR TEST_STATUS="$expected" "$sandbox/repo/scripts/test.sh" || status=$?
    [[ "$status" == "$expected" ]] || fail "cleanup changed cargo's exit status: $status"
    scratch="$(cat "$GUEST_ROOT/path")"
    [[ ! -e "$scratch" ]] || fail "owned runner scratch survived"
    [[ ! -e "$GUEST_ROOT$scratch" ]] || fail "owned guest scratch survived for exit $expected"
done
[[ "$(wc -l <"$GUEST_ROOT/trims")" -eq 2 ]] || fail "guest scratch was not trimmed after both runs"

exact="$sandbox/repo/tmp/caller-scratch"
FS_EXT4_TEST_TMPDIR="$exact" TEST_STATUS=0 "$sandbox/repo/scripts/test.sh"
[[ -d "$exact" && -f "$GUEST_ROOT$exact/fixture-copy.img" ]] || fail "caller-owned scratch was changed"
[[ "$(wc -l <"$GUEST_ROOT/trims")" -eq 2 ]] || fail "caller-owned scratch triggered a trim"
[[ "$(cat "$GUEST_ROOT$sandbox/repo/test-disks/shared.img")" == fixture ]] || fail "shared guest fixture changed"
[[ "$(cat "$GUEST_ROOT$sandbox/repo/tmp/logs/oracle.log")" == evidence ]] || fail "guest evidence changed"
echo 'PASS  served test tiers clean and trim only their owned guest scratch'

cd "$sandbox/repo"
mkdir target
touch "$sandbox/base.qcow2"
export FS_EXT4_CI_VM_BASE="$sandbox/base.qcow2" FS_EXT4_CI_VM_OVERLAY="$sandbox/overlay.qcow2"
export GITHUB_JOB=test FS_EXT4_GUEST_ARCH=x86_64 CARGO_TARGET_DIR=target
probe="$repo/scripts/ci-oracle-disk-evidence.sh"
stat_allocated_bytes() {
    python3 -c 'import os, sys; print(sum(os.stat(path).st_blocks for path in sys.argv[1:]) * 512)' "$@"
}
cargo_before="$(stat_allocated_bytes target)"
status=0
output="$(bash "$probe" bash -ec 'dd if=/dev/zero of=target/compiled bs=4096 count=16 2>/dev/null; printf data >"$1"; truncate -s 8388608 "$1"; exit 17' -- "$FS_EXT4_CI_VM_OVERLAY")" || status=$?
[[ "$status" == 17 ]] || fail "oracle evidence changed the tier exit status: $status: $output"
cargo_after="$(stat_allocated_bytes target target/compiled)"
overlay_bytes="$(stat_allocated_bytes "$FS_EXT4_CI_VM_OVERLAY")"
[[ "$cargo_after" -gt "$cargo_before" && "$overlay_bytes" -gt 0 && "$overlay_bytes" -lt 8388608 ]] || fail "fixture did not exercise allocated versus apparent size"
samples="$sandbox/oracle-allocated-bytes.log"
[[ "$(cat "$samples")" == "$output" && "$(wc -l <"$samples")" -eq 2 ]] || fail "boundary samples not kept in both logs"
timestamp='[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z'
grep -Eq "^\[$timestamp\] oracle disk before cargo_allocated_bytes=$cargo_before base_allocated_bytes=0 overlay_allocated_bytes=unavailable$" "$samples" || fail "before sample did not distinguish zero from unavailable"
grep -Eq "^\[$timestamp\] oracle disk after cargo_allocated_bytes=$cargo_after base_allocated_bytes=0 overlay_allocated_bytes=$overlay_bytes$" "$samples" || fail "after sample did not report allocated bytes"
[[ "$(FS_EXT4_GUEST_ARCH=aarch64 bash "$probe" printf '%s' bypass)" == bypass ]] || fail "ARM64 emitted samples"
[[ "$(GITHUB_JOB=pilot bash "$probe" printf '%s' bypass)" == bypass ]] || fail "non-production job emitted samples"
[[ "$(cat "$samples")" == "$output" ]] || fail "out-of-scope jobs wrote evidence"
echo 'PASS  x86 oracle boundary samples record allocated bytes, distinguish unavailable from zero, and preserve status'

heartbeat="$(bash "$probe" --heartbeat)" || fail "heartbeat sample failed"
grep -Eq "^\\[$timestamp\\] oracle disk heartbeat cargo_allocated_bytes=$cargo_after base_allocated_bytes=0 overlay_allocated_bytes=$overlay_bytes$" <<<"$heartbeat" ||
    fail "heartbeat did not report current allocated bytes"
[[ "$(tail -n 1 "$samples")" == "$heartbeat" && "$(wc -l <"$samples")" -eq 3 ]] ||
    fail "heartbeat was not appended to the boundary log"
[[ -z "$(FS_EXT4_GUEST_ARCH=aarch64 bash "$probe" --heartbeat)" ]] || fail "ARM64 emitted heartbeat evidence"
[[ -z "$(GITHUB_JOB=pilot bash "$probe" --heartbeat)" ]] || fail "pilot emitted heartbeat evidence"
[[ "$(wc -l <"$samples")" -eq 3 ]] || fail "out-of-scope heartbeats wrote evidence"
echo 'PASS  x86 oracle heartbeat samples reach both logs during the job'
