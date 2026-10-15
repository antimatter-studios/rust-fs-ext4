#!/usr/bin/env bash
# Which CI profile does this event run? Prints `quick` or `full`.
#
# A pull request -- every layer of a stack -- runs the quick profile: the
# unit tier, lint, semver, msrv and provenance, which need no VM and no
# fixture. The merge queue, a push to main or a tag, a manual dispatch, a
# workflow_call and any event this script does not know run the full
# profile: every VM, fixture, oracle, wasm and CLI job as before. What lands
# on main is decided by the merge queue's full run, never by the quick one.
set -euo pipefail
event="${1:-}"
case "$event" in
  pull_request) echo quick ;;
  *) echo full ;;
esac
