#!/usr/bin/env bash
#
# ci-c-toolchain.sh [package...] — install build-essential, and any other
# packages named, on a self-hosted Linux runner, which lacks the C toolchain
# and tools the hosted image has.
#
# Ubuntu's mirrors answer an index that is mid-sync with "File has unexpected
# size ... Mirror sync in progress?", and one such answer used to fail the job
# before anything under test ran. apt retries each download itself, and the
# whole update is tried again after a pause, a minute at most.
set -euo pipefail

for wait in 15 30 60 0; do
    if sudo apt-get -o Acquire::Retries=3 update; then
        break
    fi
    [ "$wait" != 0 ] || { echo "::error::apt-get update failed four times" >&2; exit 1; }
    echo "::warning::apt-get update failed (a mirror mid-sync?); trying again in ${wait}s" >&2
    sleep "$wait"
done
sudo apt-get -o Acquire::Retries=3 install -y build-essential "$@"
