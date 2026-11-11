#!/usr/bin/env bash
# Which CI profile does this event run? Prints `full`.
#
# Every pull request runs the full profile, regardless of title. Documentation-
# only changes may still skip the fixture and VM-dependent jobs through the
# separate path classifier in ci.yml. All other events run the full profile too.
set -euo pipefail
event="${1:-}"
echo full
