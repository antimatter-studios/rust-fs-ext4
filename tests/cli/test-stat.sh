# fs.ext4 stat, as installed: the shared keys in the shared order and
# typed, ext4's own under `ext4`, the values `write` and `mkdir` produced,
# --text, the refusals, and an image left exactly as it was. What the
# values must be on a volume Linux wrote is tests/cli_stat_kernel.rs.
# xattr, the other per-path inspection, is checked here the same way.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/stat.img"
mkfs.ext4 -q --text --size 64M "$img" >/dev/null 2>&1
check "mkfs.ext4 made the image" test -s "$img"
fs.ext4 "$img" mkdir /d >/dev/null 2>&1
check "mkdir /d exits 0" test $? -eq 0
head -c 5000 /dev/urandom >"$SANDBOX/src"
fs.ext4 "$img" write /f <"$SANDBOX/src" >/dev/null 2>&1
check "write /f exits 0" test $? -eq 0
cp "$img" "$SANDBOX/before.img"

shared='["path","inode","type","mode","uid","gid","nlink","size","allocated_bytes","atime","atime_nsec","mtime","mtime_nsec","ctime","ctime_nsec","btime","btime_nsec"]'
typed='(.path|type)=="string" and (.inode|type)=="number" and (.type|type)=="string"
    and (.mode|test("^[0-7]{4}$")) and (.uid|type)=="number" and (.gid|type)=="number"
    and (.nlink|type)=="number" and (.size|type)=="number" and (.allocated_bytes|type)=="number"
    and (.atime|type)=="number" and (.mtime|type)=="number" and (.ctime|type)=="number"
    and all(.atime_nsec, .mtime_nsec, .ctime_nsec, .btime, .btime_nsec; type=="number" or type=="null")
    and (.ext4|type)=="object" and (.ext4.flags|type)=="array" and (.ext4.generation|type)=="number"'

for path in / /d /f; do
    fs.ext4 "$img" stat "$path" >"$SANDBOX/stat.json" 2>"$SANDBOX/stat.err"
    rc=$?
    check "stat $path exits 0 ($(cat "$SANDBOX/stat.err"))" test "$rc" -eq 0
    jq_check "stat $path starts with the shared keys, in order" \
        "[keys_unsorted[]] | .[0:17] == $shared" "$SANDBOX/stat.json"
    jq_check "stat $path has every shared key typed" "$typed" "$SANDBOX/stat.json"
    jq_check "stat $path reports the path it was asked" ".path == \"$path\"" "$SANDBOX/stat.json"
done

fs.ext4 "$img" stat / >"$SANDBOX/root.json" 2>/dev/null
jq_check "stat / is the root directory, inode 2" '.type == "dir" and .inode == 2' "$SANDBOX/root.json"
fs.ext4 "$img" stat /d >"$SANDBOX/d.json" 2>/dev/null
jq_check "stat /d is an empty directory: two links" '.type == "dir" and .nlink == 2' "$SANDBOX/d.json"
fs.ext4 "$img" stat /f >"$SANDBOX/f.json" 2>/dev/null
jq_check "stat /f is a 5000-byte file with one link" \
    '.type == "file" and .size == 5000 and .nlink == 1 and .allocated_bytes >= 5000' "$SANDBOX/f.json"
fs.ext4 "$img" ls / >"$SANDBOX/ls.json" 2>/dev/null
check "stat /f and ls / agree on the inode and the mode" \
    test "$(jq -c '[.inode, .mode]' "$SANDBOX/f.json")" = "$(jq -c '.[] | select(.name == "f") | [.inode, .mode]' "$SANDBOX/ls.json")"

fs.ext4 "$img" stat --text /f >"$SANDBOX/f.txt" 2>/dev/null
check "stat --text /f exits 0" test $? -eq 0
check "stat --text /f prints key: value lines" grep -qx 'size: 5000' "$SANDBOX/f.txt"
check "stat --text /f names the type" grep -qx 'type: file' "$SANDBOX/f.txt"

# Refusals: a structured error, nothing on stdout.
fs.ext4 "$img" stat /missing >"$SANDBOX/m.out" 2>"$SANDBOX/m.err"
check "stat of a missing path exits 1" test $? -eq 1
check "stat of a missing path prints nothing on stdout" test ! -s "$SANDBOX/m.out"
jq_check "stat of a missing path is the not_found kind" '.code == 1 and .kind == "not_found"' "$SANDBOX/m.err"
fs.ext4 "$img" stat >"$SANDBOX/u.out" 2>"$SANDBOX/u.err"
check "stat with no path is a usage error" test $? -eq 2
check "stat with no path prints nothing on stdout" test ! -s "$SANDBOX/u.out"

# xattr: a path with no attributes lists `[]` and --text says nothing; a
# missing attribute or path is a structured error with nothing on stdout.
# What a populated listing and every value must be, on a volume Linux
# wrote, is tests/cli_xattr_kernel.rs.
for path in / /d /f; do
    fs.ext4 "$img" xattr "$path" >"$SANDBOX/x.json" 2>"$SANDBOX/x.err"
    rc=$?
    check "xattr $path exits 0 ($(cat "$SANDBOX/x.err"))" test "$rc" -eq 0
    jq_check "xattr $path lists no attributes" '. == []' "$SANDBOX/x.json"
done
fs.ext4 "$img" xattr --text /f >"$SANDBOX/x.txt" 2>"$SANDBOX/x.err"
rc=$?
check "xattr --text /f exits 0" test "$rc" -eq 0
check "xattr --text /f prints nothing for no attributes" test ! -s "$SANDBOX/x.txt"
for args in "/f user.missing" "/missing" "/missing user.missing"; do
    # shellcheck disable=SC2086 # each case is a fixed list of operands
    fs.ext4 "$img" xattr $args >"$SANDBOX/x.out" 2>"$SANDBOX/x.err"
    rc=$?
    check "xattr $args exits 1" test "$rc" -eq 1
    check "xattr $args prints nothing on stdout" test ! -s "$SANDBOX/x.out"
    jq_check "xattr $args is a structured error" '.code == 1 and (.error | type) == "string"' "$SANDBOX/x.err"
done
fs.ext4 "$img" xattr /f user.missing -o "$SANDBOX/value" >"$SANDBOX/x.out" 2>"$SANDBOX/x.err"
rc=$?
check "xattr -o of a missing attribute exits 1" test "$rc" -eq 1
check "the error names the missing attribute" grep -q 'user\.missing' "$SANDBOX/x.err"
check "a missing attribute creates no output file" test ! -e "$SANDBOX/value"
check "a missing attribute leaves no partial file" test ! -e "$SANDBOX/value.partial"
for args in "" "/f -o value" "/f user.a extra"; do
    # shellcheck disable=SC2086 # each case is a fixed list of operands
    fs.ext4 "$img" xattr $args >"$SANDBOX/x.out" 2>"$SANDBOX/x.err"
    rc=$?
    check "xattr $args is a usage error" test "$rc" -eq 2
    check "xattr $args as a usage error prints nothing on stdout" test ! -s "$SANDBOX/x.out"
done

check "stat and xattr left the image as they found it" cmp -s "$img" "$SANDBOX/before.img"

finish
