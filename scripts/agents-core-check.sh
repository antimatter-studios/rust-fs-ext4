#!/usr/bin/env bash
# Verify AGENTS.md carries the shared agent-core block, unmodified.
#
# Every repository in this family embeds the same block between BEGIN/END
# markers, so an agent moving between them is told the same rules. This script
# is what stops that from quietly ceasing to be true: it hashes the content
# between the markers and compares it against the canonical digest below.
#
# The digest covers ONLY the content between the markers -- never the marker
# lines themselves, which carry the digest and so cannot be part of it.
#
# WHEN THE BLOCK CHANGES, it changes everywhere: edit the canonical copy, then
# update the digest in this script and in the BEGIN marker of every repository
# that carries it. A repo left behind fails this check rather than silently
# running last month's rules.
set -euo pipefail

EXPECTED_SHA256="8e0e4d55b026cff6cc3476160ed112117ca8ea64ad456408b9f1fe2a1dcf308b"
BEGIN='<!-- BEGIN SHARED BLOCK: agent-core v2'
END='<!-- END SHARED BLOCK: agent-core v2 -->'

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FILE="$ROOT/AGENTS.md"

die() { echo "agents-core-check: $*" >&2; exit 1; }

[ -f "$FILE" ] || die "AGENTS.md is missing"
grep -qF "$BEGIN" "$FILE" || die "AGENTS.md has no agent-core BEGIN marker"
grep -qF "$END"   "$FILE" || die "AGENTS.md has no agent-core END marker"

body="$(awk -v b="$BEGIN" -v e="$END" '
    index($0, b) == 1 { inblock = 1; next }
    index($0, e) == 1 { inblock = 0; next }
    inblock { print }
' "$FILE")"

[ -n "$body" ] || die "the agent-core block is empty"

actual="$(printf '%s\n' "$body" | sha256sum | awk '{print $1}')"
if [ "$actual" != "$EXPECTED_SHA256" ]; then
    die "the agent-core block does not match the canonical copy.
  expected $EXPECTED_SHA256
  actual   $actual
  Either this repository is behind, or the block was edited here instead of
  at the canonical copy. The block is shared: change it everywhere or nowhere."
fi

# The marker must agree with the content, or the marker is decoration.
declared="$(grep -oE 'sha256:[0-9a-f]{64}' "$FILE" | head -1 | cut -d: -f2)"
[ "$declared" = "$EXPECTED_SHA256" ] || die "the BEGIN marker declares sha256:$declared but the canonical digest is $EXPECTED_SHA256"

echo "agents-core-check: AGENTS.md carries agent-core v2, unmodified"
