#!/usr/bin/env bash
# scripts/copy-sparse.sh copies a fixture with GNU cp's --sparse=always where cp is
# GNU's, and with a plain cp where it is BSD's (macOS), which refuses the option.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
copy="$REPO/scripts/copy-sparse.sh"
mkdir -p "$REPO/tmp"
sandbox="$(mktemp -d "$REPO/tmp/copy-sparse-check.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT
fail() { echo "FAIL  $*" >&2; exit 1; }

printf 'image\n' > "$sandbox/source.img"
mkdir "$sandbox/gnu" "$sandbox/bsd"
# A GNU cp answers --version and takes --sparse=always.
cat > "$sandbox/gnu/cp" <<'STUB'
#!/bin/sh
[ "$1" = --version ] && { echo 'cp (GNU coreutils) 9.4'; exit 0; }
printf '%s\n' "$*" > "$(dirname "$0")/args"
[ "$1" = --sparse=always ] || exit 70
exec /bin/cp "$2" "$3"
STUB
# A BSD cp refuses every long option, as macOS's does.
cat > "$sandbox/bsd/cp" <<'STUB'
#!/bin/sh
case "$1" in --*) echo "cp: illegal option -- -" >&2; exit 64 ;; esac
printf '%s\n' "$*" > "$(dirname "$0")/args"
exec /bin/cp "$1" "$2"
STUB
chmod +x "$sandbox/gnu/cp" "$sandbox/bsd/cp"

PATH="$sandbox/gnu:$PATH" bash "$copy" "$sandbox/source.img" "$sandbox/gnu.img" ||
    fail "a GNU cp copy failed"
[ "$(cat "$sandbox/gnu/args")" = "--sparse=always $sandbox/source.img $sandbox/gnu.img" ] ||
    fail "a GNU cp was not asked to keep the holes: $(cat "$sandbox/gnu/args")"
cmp -s "$sandbox/source.img" "$sandbox/gnu.img" || fail "the GNU copy differs"

PATH="$sandbox/bsd:$PATH" bash "$copy" "$sandbox/source.img" "$sandbox/bsd.img" ||
    fail "a BSD cp copy failed: macOS's cp refuses --sparse=always"
[ "$(cat "$sandbox/bsd/args")" = "$sandbox/source.img $sandbox/bsd.img" ] ||
    fail "a BSD cp was given an option it refuses: $(cat "$sandbox/bsd/args")"
cmp -s "$sandbox/source.img" "$sandbox/bsd.img" || fail "the BSD copy differs"

bash "$copy" "$sandbox/source.img" "$sandbox/host.img" || fail "this host's cp failed"
cmp -s "$sandbox/source.img" "$sandbox/host.img" || fail "this host's copy differs"

# shellcheck disable=SC2016  # the literal line, not an expansion
grep -q 'bash scripts/copy-sparse.sh "$img" "test-disks/$base.partial"' "$REPO/test-disks/build-fixtures.sh" ||
    fail "test-disks/build-fixtures.sh does not copy its images through scripts/copy-sparse.sh"
echo "ok    copy-sparse: GNU keeps holes, BSD (macOS) copies, this host copies"
