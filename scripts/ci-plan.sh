#!/usr/bin/env bash
# Which CI profile does this event run? Prints `quick` or `full`.
#
# A pull request -- every layer of a stack -- runs the quick profile: the
# unit tier, lint, semver, msrv and provenance, which need no VM and no
# fixture. A pull request whose title starts "red:" (the second argument)
# runs the full profile: it holds only a failing test, and its run is the red
# evidence, so a kernel, VM or fixture test it adds must run there to fail.
# The match ignores case, as GitHub's startsWith does where ci.yml keeps that
# run uncancelled. The merge queue, a push to main or a tag, a manual dispatch, a
# workflow_call and any event this script does not know run the full
# profile: every VM, fixture, oracle, wasm and CLI job as before. What lands
# on main is decided by the merge queue's full run, never by the quick one.
set -euo pipefail
event="${1:-}"
title="${2:-}"
shopt -s nocasematch
case "$event" in
  pull_request)
    case "$title" in
      red:*) echo full ;;
      *) echo quick ;;
    esac ;;
  *) echo full ;;
esac
