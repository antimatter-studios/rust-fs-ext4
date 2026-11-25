#!/usr/bin/env bash
#
# copy-sparse.sh <source> <destination> — copy a disk image and keep its holes.
#
# GNU cp writes the holes back with --sparse=always. macOS's cp is BSD's, which
# has no such option and refuses it, so `chore fixtures` failed on a Mac after the
# guest had built every image. On APFS that cp clones the file, which keeps the
# holes anyway. GNU cp answers --version and BSD cp refuses it, which is how the
# two are told apart: a Mac with GNU coreutils first on PATH gets the GNU copy.
set -euo pipefail

[ "$#" = 2 ] || { echo "usage: copy-sparse.sh <source> <destination>" >&2; exit 2; }
if cp --version >/dev/null 2>&1; then
    exec cp --sparse=always "$1" "$2"
fi
exec cp "$1" "$2"
