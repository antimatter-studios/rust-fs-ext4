#!/usr/bin/env bash
set -euo pipefail

matches=$(rg -n --no-heading --glob '*.rs' \
    '(^|[^[:alnum:]_])(?:std::)?fs::read[[:space:]]*\(|\.read_to_end[[:space:]]*\(' tests || true)
allowed=$(printf '%s\n' "$matches" \
    | rg '^tests/support/src/lib.rs:[0-9]+:[[:space:]]*fs::read\(path\)$' || true)
unexpected=$(printf '%s\n' "$matches" \
    | rg -v '^tests/support/src/lib.rs:[0-9]+:[[:space:]]*fs::read\(path\)$' || true)

if [ -n "$unexpected" ]; then
    printf 'whole-file reads are forbidden in tests; use bounded helpers:\n%s\n' "$unexpected" >&2
    exit 1
fi

allowed_count=$(printf '%s\n' "$allowed" | rg -c . || true)
if [ "$allowed_count" -ne 1 ]; then
    printf 'expected exactly one bounded fs::read implementation, found %s\n' "$allowed_count" >&2
    exit 1
fi
