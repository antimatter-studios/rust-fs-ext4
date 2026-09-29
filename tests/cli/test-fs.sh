# fs.ext4's read verbs on an image our own mkfs.ext4 makes: ls, read,
# get/info with the canonical keys and their types, the verbs that answer
# `not implemented`, and structured errors with nothing on stdout.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/fs.img"
mkfs.ext4 -q --size 64M --label CLITEST "$img" >/dev/null 2>&1
check "mkfs.ext4 made the image" test -s "$img"

# ls /: a fresh image of ours has an empty root -- no lost+found yet
# (#443; mke2fs makes one). Flip this when the formatter does.
fs.ext4 "$img" ls / >"$SANDBOX/ls.json" 2>"$SANDBOX/ls.err"
check "ls / exits 0 ($(cat "$SANDBOX/ls.err"))" test $? -eq 0
jq_check "ls / of a fresh image is an empty array (#443)" '. == []' "$SANDBOX/ls.json"
fs.ext4 "$img" ls --text / >"$SANDBOX/ls.txt" 2>/dev/null
check "ls --text / of a fresh image prints nothing" test ! -s "$SANDBOX/ls.txt"
fs.ext4 "$img" ls /missing >"$SANDBOX/lsm.out" 2>"$SANDBOX/lsm.err"
check "ls of a missing path exits 1" test $? -eq 1
check "ls of a missing path prints nothing on stdout" test ! -s "$SANDBOX/lsm.out"
jq_check "ls of a missing path says not found" '.code == 1 and (.error | test("not found"))' "$SANDBOX/lsm.err"

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

# Verbs the library cannot do yet: status 3, `not implemented`, nothing on stdout.
for verb in "set label X" "resize 128M"; do
    # shellcheck disable=SC2086  # the words are the point
    fs.ext4 "$img" $verb >"$SANDBOX/ni.out" 2>"$SANDBOX/ni.err"
    check "$verb exits 3" test $? -eq 3
    check "$verb prints nothing on stdout" test ! -s "$SANDBOX/ni.out"
    jq_check "$verb says not implemented" '.code == 3 and (.error | startswith("not implemented"))' "$SANDBOX/ni.err"
done

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
