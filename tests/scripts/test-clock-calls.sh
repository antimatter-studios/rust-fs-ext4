#!/usr/bin/env bash
# The clock and the process ID are read in ONE place: src/runtime.rs.
#
# On wasm32-unknown-unknown -- the browser build -- `std` compiles
# `SystemTime::now()`, `Instant::now()` and `std::process::id()` and PANICS
# when they are called. A build cannot see that, and the wasm tier
# (`chore test:wasm`) only sees the paths its tests happen to reach. This
# sees every call, needs no toolchain, and names the approved place for one:
# src/runtime.rs, which has a browser path for each.
#
# Test modules are excepted -- they run natively -- from a `#[cfg(test)]`
# line to the brace that closes the item after it. Line comments are
# ignored, so prose may name the calls.
#
# TRACKED EXCEPTIONS. Known call sites with an open issue, each pinned to
# the number of calls its file holds today. The guard fails when a NEW call
# appears (in any file, or a further one in a listed file) and ALSO when a
# listed call is fixed and still listed: the fix removes its line here.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PATTERN='SystemTime::now|Instant::now|process::id'
ALLOWED='runtime.rs'

# file<TAB>call<TAB>count<TAB>issue -- none open today. An entry names a
# known call site with an issue to remove it, e.g. `mkfs.rs SystemTime::now 1
# '#294'`, and is deleted by the fix.
EXCEPTIONS=""

# scan DIR: print `relative-path<TAB>call<TAB>line` for every call outside
# a test module and outside the allowed file.
scan() {
    local dir="$1" f rel
    while IFS= read -r f; do
        rel="${f#"$dir"/}"
        [ "$rel" = "$ALLOWED" ] && continue
        awk -v rel="$rel" -v pat="$PATTERN" '
            function braces(s,   o, c) {
                o = gsub(/\{/, "{", s); c = gsub(/\}/, "}", s); return o - c
            }
            {
                line = $0
                sub(/\/\/.*/, "", line)
                if (skipping) {
                    depth += braces(line)
                    if (opened && depth <= 0) { skipping = 0 }
                    else if (depth > 0) { opened = 1 }
                    # `#[cfg(test)] use ...;` -- an item with no body.
                    else if (line ~ /;[ \t]*$/) { skipping = 0 }
                    next
                }
                if (line ~ /^[ \t]*#\[cfg\(test\)\]/) {
                    skipping = 1; depth = 0; opened = 0; next
                }
                s = line
                while (match(s, pat)) {
                    printf "%s\t%s\t%d\n", rel, substr(s, RSTART, RLENGTH), NR
                    s = substr(s, RSTART + RLENGTH)
                }
            }
        ' "$f" || return 1
    done < <(find "$dir" -name '*.rs' | LC_ALL=C sort)
}

# check DIR EXCEPTIONS: print one line per violation; silent when clean.
check() {
    local dir="$1" exceptions="$2" hits
    hits="$(scan "$dir")" || return 1
    # Every call is either excepted or a violation.
    printf '%s\n' "$hits" | EXCEPTIONS_TABLE="$exceptions" awk -F'\t' '
        BEGIN {
            n = split(ENVIRON["EXCEPTIONS_TABLE"], rows, "\n")
            for (i = 1; i <= n; i++) {
                if (rows[i] == "") continue
                split(rows[i], r, "\t")
                want[r[1] "\t" r[2]] = r[3]; issue[r[1] "\t" r[2]] = r[4]
            }
        }
        $0 == "" { next }
        {
            key = $1 "\t" $2
            got[key]++
            if (!(key in want)) {
                printf "src/%s:%s calls %s; read it through src/runtime.rs, which works in the browser build\n", $1, $3, $2
            }
        }
        END {
            for (key in want) {
                split(key, k, "\t")
                if (!(key in got)) {
                    printf "src/%s no longer calls %s: remove its tracked exception (%s) from this guard\n", k[1], k[2], issue[key]
                } else if (got[key] != want[key]) {
                    printf "src/%s calls %s %d times; the tracked exception (%s) allows %d\n", k[1], k[2], got[key], issue[key], want[key]
                }
            }
        }
    '
}

fails=0
note() { echo "FAIL  $*" >&2; fails=$(( fails + 1 )); }

# --- 1. The tree. ------------------------------------------------------------
out="$(check "$REPO/src" "$EXCEPTIONS")" || note "the scan itself failed: $out"
if [ -n "$out" ]; then
    while IFS= read -r line; do note "$line"; done <<< "$out"
fi

# --- 2. The guard can fail. --------------------------------------------------
# A check that has never been seen to refuse anything is indistinguishable
# from no check, so each shape it exists to catch is driven here.
mkdir -p "$REPO/tmp"
work="$(mktemp -d "$REPO/tmp/clock-calls-test.XXXXXX")"
trap 'rm -rf "$work"' EXIT
one='a.rs\tInstant::now\t1\t#1'

mkdir -p "$work/clean"
printf 'fn f() {}\n' > "$work/clean/a.rs"
printf 'fn now() { std::time::SystemTime::now(); std::process::id(); }\n' > "$work/clean/runtime.rs"
[ -z "$(check "$work/clean" "")" ] || note "self-test: a call in runtime.rs was refused"

mkdir -p "$work/new"
printf 'fn f() {\n    let t = std::time::Instant::now();\n}\n' > "$work/new/a.rs"
case "$(check "$work/new" "")" in
    *"a.rs:2 calls Instant::now"*) ;;
    *) note "self-test: a new Instant::now call was not refused" ;;
esac

mkdir -p "$work/test_mod"
printf 'fn f() {}\n// SystemTime::now in prose\n#[cfg(test)]\nmod tests {\n    fn g() {\n        std::process::id();\n    }\n}\n' > "$work/test_mod/a.rs"
[ -z "$(check "$work/test_mod" "")" ] || note "self-test: a call in a test module or a comment was refused"

mkdir -p "$work/after_test_mod"
printf '#[cfg(test)]\nmod tests {\n    fn g() {}\n}\nfn f() { std::process::id(); }\n' > "$work/after_test_mod/a.rs"
case "$(check "$work/after_test_mod" "")" in
    *"calls process::id"*) ;;
    *) note "self-test: a call after a test module was not refused" ;;
esac

mkdir -p "$work/fixed"
printf 'fn f() {}\n' > "$work/fixed/a.rs"
case "$(check "$work/fixed" "$(printf "$one")")" in
    *"remove its tracked exception"*) ;;
    *) note "self-test: an exception whose call was fixed was not refused" ;;
esac

mkdir -p "$work/more"
printf 'fn f() { Instant::now(); Instant::now(); }\n' > "$work/more/a.rs"
case "$(check "$work/more" "$(printf "$one")")" in
    *"2 times"*) ;;
    *) note "self-test: a second call beside a tracked one was not refused" ;;
esac

if [ "$fails" -gt 0 ]; then
    exit 1
fi
echo "PASS  src/ reads the clock and the process ID only through src/runtime.rs"
