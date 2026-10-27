#!/usr/bin/env bash
# The CI profile's plan and verdict, graded as a truth table.
#
# scripts/ci-plan.sh says which profile an event runs: a pull request runs
# the quick one, unless its title starts "red:", and every other event the
# full one. scripts/ci-ok.sh is the
# ci-ok verdict: it accepts a skipped job only where that plan skipped it,
# and refuses a missing or invalid plan, a missing job, and any failure or
# cancellation. Each row is pinned here so a profile cannot quietly drop a
# leg. This runs in the `changes` job, on the host shell the scripts run in;
# tests/ci_plan.rs holds the table's schema and checks this wiring.
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
fails=0
checks=0
ok() { checks=$((checks + 1)); }
fail() {
  checks=$((checks + 1))
  fails=$((fails + 1))
  echo "FAIL: $*" >&2
}

ALL=(changes unit fixtures test test-arm64 suite-in-vm wasm semver msrv cli provenance)
FULL_ONLY=(fixtures test test-arm64 suite-in-vm wasm cli)
VM_ONLY=(fixtures test test-arm64 suite-in-vm)
QUICK=(changes unit semver msrv provenance)

in_list() {
  local x="$1" y
  shift
  for y in "$@"; do [[ "$x" == "$y" ]] && return 0; done
  return 1
}

# rows PROFILE CODE [JOB RESULT|JOB --omit]: what each job reports when the
# plan is followed exactly, with JOB's result replaced or JOB left out.
rows() {
  local profile="$1" code="$2" target="${3-}" result="${4-}" job r
  for job in "${ALL[@]}"; do
    if [[ -n "$target" && "$job" == "$target" && "$result" == "--omit" ]]; then continue; fi
    r=success
    if { [[ "$profile" == quick ]] && in_list "$job" "${FULL_ONLY[@]}"; } ||
      { [[ "$code" == false ]] && in_list "$job" "${VM_ONLY[@]}"; }; then
      r=skipped
    fi
    if [[ -n "$target" && "$job" == "$target" ]]; then r="$result"; fi
    printf '%s %s\n' "$job" "$r"
  done
}

# verdict PROFILE CODE: "<unset>" leaves the variable out of the environment.
verdict() {
  local -a e=(env -u PROFILE -u CODE)
  [[ "$1" != "<unset>" ]] && e+=("PROFILE=$1")
  [[ "$2" != "<unset>" ]] && e+=("CODE=$2")
  "${e[@]}" bash "$here/ci-ok.sh" >/dev/null 2>&1
}

# expect pass|fail DESCRIPTION VPROFILE VCODE RPROFILE RCODE [JOB RESULT]
expect() {
  local want="$1" desc="$2" vp="$3" vc="$4"
  shift 4
  if rows "$@" | verdict "$vp" "$vc"; then
    [[ "$want" == pass ]] && ok || fail "$desc must fail"
  else
    [[ "$want" == fail ]] && ok || fail "$desc must pass"
  fi
}

# only a pull request runs the quick profile
got="$(bash "$here/ci-plan.sh" pull_request)" && [[ "$got" == quick ]] && ok || fail "pull_request planned '$got', want quick"
for event in merge_group push workflow_dispatch workflow_call schedule release "" pull_request_target; do
  got="$(bash "$here/ci-plan.sh" "$event")" && [[ "$got" == full ]] && ok ||
    fail "event '$event' planned '$got', must run the full profile"
done

# a red: pull request runs the full profile: its run is the red evidence, so a
# kernel, VM or fixture test it adds must run there to fail. GitHub's
# startsWith, which keeps the run uncancelled, ignores case; so does the plan.
for title in "red: a kernel test fails" "red:" "Red: a capital" "RED: shouting"; do
  got="$(bash "$here/ci-plan.sh" pull_request "$title")" && [[ "$got" == full ]] && ok ||
    fail "pull_request titled '$title' planned '$got', want full"
done
for title in "" "fix: red: later" " red: a leading space" "redo: not red" "red a missing colon"; do
  got="$(bash "$here/ci-plan.sh" pull_request "$title")" && [[ "$got" == quick ]] && ok ||
    fail "pull_request titled '$title' planned '$got', want quick"
done
for event in merge_group push workflow_dispatch; do
  got="$(bash "$here/ci-plan.sh" "$event" "fix: not red")" && [[ "$got" == full ]] && ok ||
    fail "event '$event' with a title planned '$got', must run the full profile"
done

# every planned outcome is green
for profile in quick full; do
  for code in true false; do
    expect pass "profile $profile, code $code: the planned outcome" "$profile" "$code" "$profile" "$code"
  done
done

# a full run that skips a full-only job fails
for job in "${FULL_ONLY[@]}"; do
  expect fail "full run with $job skipped" full true full true "$job" skipped
done
for job in wasm cli; do
  expect fail "documentation-only full run with $job skipped" full false full false "$job" skipped
done

# a quick run never accepts a skipped quick job
for job in "${QUICK[@]}"; do
  for profile in quick full; do
    expect fail "$profile run with $job skipped" "$profile" true "$profile" true "$job" skipped
  done
done

# a failed, cancelled or missing job fails every profile
for profile in quick full; do
  for code in true false; do
    for job in "${ALL[@]}"; do
      for result in failure cancelled "" neutral; do
        expect fail "$profile/$code: $job '$result'" "$profile" "$code" "$profile" "$code" "$job" "$result"
      done
      expect fail "$profile/$code: missing $job" "$profile" "$code" "$profile" "$code" "$job" --omit
    done
  done
done

# a missing or invalid plan fails
expect fail "unset profile" "<unset>" true full true
expect fail "empty profile" "" true full true
expect fail "profile fast" fast true full true
expect fail "unset code" full "<unset>" full true
expect fail "code maybe" full maybe full true

if ((fails > 0)); then
  echo "test-ci-plan.sh: $fails of $checks checks failed" >&2
  exit 1
fi
echo "test-ci-plan.sh: $checks checks"
echo "test-ci-plan.sh: all checks passed"
