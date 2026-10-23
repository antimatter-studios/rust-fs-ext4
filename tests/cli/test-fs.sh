# fs.ext4's read verbs on an image our own mkfs.ext4 makes: ls, read,
# get/info with the canonical keys and their types, set label, the verb that
# answers `not implemented`, and structured errors with nothing on stdout.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/fs.img"
mkfs.ext4 -q --size 64M --label CLITEST "$img" >/dev/null 2>&1
check "mkfs.ext4 made the image" test -s "$img"

# ls /: a fresh image of ours holds lost+found and nothing else, as one
# mke2fs made would (#443).
fs.ext4 "$img" ls / >"$SANDBOX/ls.json" 2>"$SANDBOX/ls.err"
rc=$?
check "ls / exits 0 ($(cat "$SANDBOX/ls.err"))" test "$rc" -eq 0
jq_check "ls / of a fresh image is lost+found alone (#443)" \
    '. | length == 1 and .[0].name == "lost+found" and .[0].type == "dir" and .[0].inode == 11 and .[0].mode == "0700"' \
    "$SANDBOX/ls.json"
fs.ext4 "$img" ls --text / >"$SANDBOX/ls.txt" 2>/dev/null
check "ls --text / of a fresh image names lost+found alone" \
    test "$(awk '{print $NF}' "$SANDBOX/ls.txt")" = "lost+found"
fs.ext4 "$img" ls /missing >"$SANDBOX/lsm.out" 2>"$SANDBOX/lsm.err"
check "ls of a missing path exits 1" test $? -eq 1
check "ls of a missing path prints nothing on stdout" test ! -s "$SANDBOX/lsm.out"
jq_check "ls of a missing path is the not_found kind" '.code == 1 and .kind == "not_found"' "$SANDBOX/lsm.err"

# A path that does not exist is rust-fs-core's not_found kind, exit 1, on
# every verb that reads one, whatever its message says.
not_found() { # <what> <args...>
    local what="$1"
    shift
    fs.ext4 "$img" "$@" >"$SANDBOX/nf.out" 2>"$SANDBOX/nf.err"
    local rc=$?
    check "$what of a missing path exits 1, not $rc" test "$rc" -eq 1
    jq_check "$what of a missing path is the not_found kind" '.code == 1 and .kind == "not_found"' "$SANDBOX/nf.err"
}
not_found read read /missing
not_found "read of a missing parent" read /missing/f
not_found export export /missing "$SANDBOX/nf-export"
not_found "export --recursive" export --recursive /missing "$SANDBOX/nf-tree"

# get / info: every canonical key, typed; get and info identical.
fs.ext4 "$img" get >"$SANDBOX/get.json" 2>/dev/null
check "get exits 0" test $? -eq 0
jq_check "get carries every canonical key with its type" \
    '(.fs=="ext4") and (.label|type)=="string" and (.total_bytes|type)=="number" and (.free_bytes|type)=="number" and (.block_size|type)=="number" and (.dirty|type)=="boolean" and (.ext4|type)=="object"' \
    "$SANDBOX/get.json"
jq_check "the volume is clean and the size asked for" '.dirty == false and .total_bytes == 67108864' "$SANDBOX/get.json"
fs.ext4 "$img" info >"$SANDBOX/info.json" 2>/dev/null
check "info and get print the same" cmp -s "$SANDBOX/get.json" "$SANDBOX/info.json"
fs.ext4 "$img" get label >"$SANDBOX/label.json" 2>/dev/null
jq_check "get label is {\"label\": \"CLITEST\"}" '. == {"label": "CLITEST"}' "$SANDBOX/label.json"
check "get label --text is CLITEST" test "$(fs.ext4 "$img" get label --text)" = CLITEST
check "get ext4.uuid --text is a UUID" \
    grep -qE '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' <<<"$(fs.ext4 "$img" get ext4.uuid --text)"

# set label rewrites the label, and get reads the new one back.
fs.ext4 "$img" set label RELABELLED >"$SANDBOX/set.json" 2>/dev/null
check "set label exits 0" test $? -eq 0
jq_check "set label reports the new label" '. == {"label": "RELABELLED"}' "$SANDBOX/set.json"
check "get label --text is the new label" test "$(fs.ext4 "$img" get label --text)" = RELABELLED
fs.ext4 "$img" set label CLITEST >/dev/null 2>&1
check "set label puts the old label back" test "$(fs.ext4 "$img" get label --text)" = CLITEST

# A verb the library cannot do: status 3, `not implemented`, nothing on stdout.
fs.ext4 "$img" resize 128M >"$SANDBOX/ni.out" 2>"$SANDBOX/ni.err"
check "resize exits 3" test $? -eq 3
check "resize prints nothing on stdout" test ! -s "$SANDBOX/ni.out"
jq_check "resize says not implemented" '.code == 3 and (.error | startswith("not implemented"))' "$SANDBOX/ni.err"

# Failures: status 1, a structured error, nothing on stdout.
fs.ext4 "$img" read / >"$SANDBOX/dir.out" 2>"$SANDBOX/dir.err"
check "read of a directory exits 1" test $? -eq 1
check "read of a directory prints nothing on stdout" test ! -s "$SANDBOX/dir.out"
jq_check "read of a directory says so" '.code == 1 and (.error | test("is a directory"))' "$SANDBOX/dir.err"
fs.ext4 "$SANDBOX/absent.img" ls / >"$SANDBOX/absent.out" 2>"$SANDBOX/absent.err"
check "a missing image exits 1" test $? -eq 1
check "a missing image prints nothing on stdout" test ! -s "$SANDBOX/absent.out"
jq_check "a missing image is a structured error" '.code == 1' "$SANDBOX/absent.err"
head -c 4096 "$img" >"$SANDBOX/cut.img"
fs.ext4 "$SANDBOX/cut.img" info >"$SANDBOX/cut.out" 2>"$SANDBOX/cut.err"
check "a truncated image exits 1" test $? -eq 1
check "a truncated image prints nothing on stdout" test ! -s "$SANDBOX/cut.out"
jq_check "a truncated image is a structured error" '.code == 1' "$SANDBOX/cut.err"

finish
