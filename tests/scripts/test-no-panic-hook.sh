#!/usr/bin/env bash
# Nothing in src/ replaces the process-wide panic hook -- test modules
# included, which are the reason for this guard (#331).
#
# The hook belongs to the whole process, and the library's tests run in
# parallel threads of ONE process. A test that swaps the hook, even for a
# moment and even to put it back, is swapping it for every other test
# running beside it: a real failure elsewhere that panics inside that
# window loses its message, and the run says a test failed without saying
# why. `catch_unwind` does not need the hook changed to catch a panic.
#
# Line comments are ignored, so prose may name the calls.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PATTERN='set_hook|take_hook'

# scan DIR: print `relative-path:line: text` for every call in DIR/**.rs.
scan() {
    local dir="$1" f rel
    while IFS= read -r f; do
        rel="${f#"$dir"/}"
        awk -v rel="$rel" -v pat="$PATTERN" '
            {
                line = $0
                sub(/\/\/.*/, "", line)
                if (line ~ pat) { printf "%s:%d: %s\n", rel, NR, $0 }
            }
        ' "$f" || return 1
    done < <(find "$dir" -name '*.rs' | LC_ALL=C sort)
}

fails=0
note() { echo "FAIL  $*" >&2; fails=$(( fails + 1 )); }

# --- 1. The tree. ------------------------------------------------------------
out="$(scan "$REPO/src")" || note "the scan itself failed: $out"
if [ -n "$out" ]; then
    while IFS= read -r line; do
        note "src/$line -- the panic hook is process-wide; leave it alone"
    done <<< "$out"
fi

# --- 2. The guard can fail. --------------------------------------------------
mkdir -p "$REPO/tmp"
work="$(mktemp -d "$REPO/tmp/no-panic-hook-test.XXXXXX")"
trap 'rm -rf "$work"' EXIT

mkdir -p "$work/clean"
printf 'fn f() {}\n// std::panic::set_hook in prose\n' > "$work/clean/a.rs"
[ -z "$(scan "$work/clean")" ] || note "self-test: a comment naming set_hook was refused"

mkdir -p "$work/set"
printf '#[cfg(test)]\nmod tests {\n    fn g() {\n        std::panic::set_hook(Box::new(|_| {}));\n    }\n}\n' > "$work/set/a.rs"
case "$(scan "$work/set")" in
    *"a.rs:4:"*) ;;
    *) note "self-test: set_hook in a test module was not refused" ;;
esac

mkdir -p "$work/take/nested"
printf 'fn f() { let _ = std::panic::take_hook(); }\n' > "$work/take/nested/b.rs"
case "$(scan "$work/take")" in
    *"nested/b.rs:1:"*) ;;
    *) note "self-test: take_hook in a nested module was not refused" ;;
esac

if [ "$fails" -gt 0 ]; then
    exit 1
fi
echo "PASS  nothing in src/ replaces the process-wide panic hook"
