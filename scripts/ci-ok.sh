#!/usr/bin/env bash
# The ci-ok verdict. Reads `<job> <result>` lines on stdin (one per job ci-ok
# needs) and the plan from PROFILE (quick|full) and CODE (true|false).
#
# Fails on a missing or invalid plan, on a needed job that is absent, and on
# any job that failed, was cancelled, or was skipped where the plan did not
# skip it. A skip is accepted only for a full-only job under the quick
# profile, or for a VM job when the change is documentation alone.
set -uo pipefail

all="changes unit fixtures test-builder-image build-test-x86_64 build-test-aarch64 test-images-x86_64 test-oracle-x86_64 test-kernel-x86_64 test-lwext4-x86_64 test-scripts-x86_64 test-unit-aarch64 test-images-aarch64 wasm semver msrv cli provenance"
full_only=" fixtures test-builder-image build-test-x86_64 build-test-aarch64 test-images-x86_64 test-oracle-x86_64 test-kernel-x86_64 test-lwext4-x86_64 test-scripts-x86_64 test-unit-aarch64 test-images-aarch64 wasm cli "
vm_only=" fixtures test-builder-image build-test-x86_64 build-test-aarch64 test-images-x86_64 test-oracle-x86_64 test-kernel-x86_64 test-lwext4-x86_64 test-scripts-x86_64 test-unit-aarch64 test-images-aarch64 "

profile="${PROFILE:-}"
code="${CODE:-}"
case "$profile" in quick | full) ;; *) echo "::error::invalid or missing plan: profile='$profile'"; exit 1 ;; esac
case "$code" in true | false) ;; *) echo "::error::invalid or missing plan: code='$code'"; exit 1 ;; esac

input="$(cat)"

bad=()
for job in $all; do
  r="$(printf '%s\n' "$input" | awk -v j="$job" '$1 == j { print $2; exit }')"
  r="${r:-missing}"
  echo "$job: $r"
  case "$r" in
    success) ;;
    skipped)
      if [ "$job" != changes ] && [ "$profile" = quick ] && [[ "$full_only" == *" $job "* ]]; then :
      elif [ "$code" = false ] && [[ "$vm_only" == *" $job "* ]]; then :
      else bad+=("$job"); fi ;;
    *) bad+=("$job") ;;
  esac
done
# A gate job (<job>-gate) that failed or was cancelled leaves its job skipped
# or unrun: it counts as that job failing. A skipped gate is its job's skip.
while read -r name result; do
  case "$name" in
    *-gate) case "$result" in failure | cancelled) echo "$name: $result"; bad+=("$name") ;; esac ;;
  esac
done <<<"$input"
echo "profile: $profile, code changed: $code"
if [ "${#bad[@]}" -gt 0 ]; then
  echo "::error::not green: ${bad[*]} (failed, cancelled, skipped or missing)"
  exit 1
fi
echo "all required jobs succeeded"
