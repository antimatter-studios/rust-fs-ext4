# fs.ext4 mv, as installed: a name is renamed in its directory, a file is
# moved to another directory, a directory is moved with its `..` and both
# parents' link counts following it, --replace puts a file over a file and a
# directory over an empty directory, --offset reaches a filesystem inside a
# larger image and nothing before it, every refusal is a structured error
# with nothing on stdout that leaves the image byte-identical, and the volume
# is clean afterwards by fsck.ext4 and by the dirty flag.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/mv.img"
mkfs.ext4 -q --text --size 64M "$img" >/dev/null 2>&1
check "mkfs.ext4 made the image" test -s "$img"

for dir in /a /a/sub /b /empty /full /moving; do
    fs.ext4 "$img" mkdir "$dir" >/dev/null 2>&1
done
head -c 5000 /dev/urandom >"$SANDBOX/f"
head -c 7000 /dev/urandom >"$SANDBOX/g"
for file in /f /a/g /a/sub/y /full/x /moving/inner; do
    fs.ext4 "$img" write "$file" <"$SANDBOX/f" >/dev/null 2>&1
done
fs.ext4 "$img" write /old <"$SANDBOX/g" >/dev/null 2>&1
check "the names to move were written" cmp -s "$SANDBOX/f" <(fs.ext4 "$img" read /a/sub/y)

# moved SOURCE PATH [OPTION]: mv exits 0, reports {"path": PATH, "moved":
# true}, SOURCE's directory no longer has its name, and PATH's has the new
# one.
moved() {
    local from="$1" to="$2"
    shift 2
    fs.ext4 "$img" mv "$@" "$from" "$to" >"$SANDBOX/mv.json" 2>"$SANDBOX/mv.err"
    local rc=$?
    check "mv $* $from $to exits 0 ($(cat "$SANDBOX/mv.err"))" test "$rc" -eq 0
    jq_check "mv $from $to reports the new path moved" \
        ".path == \"$to\" and .moved == true and (keys | length) == 2" "$SANDBOX/mv.json"
    fs.ext4 "$img" ls "${from%/*}/" >"$SANDBOX/ls.json" 2>/dev/null
    jq_check "${from%/*}/ no longer has ${from##*/}" "all(.[]; .name != \"${from##*/}\")" "$SANDBOX/ls.json"
    fs.ext4 "$img" ls "${to%/*}/" >"$SANDBOX/ls.json" 2>/dev/null
    jq_check "${to%/*}/ has ${to##*/}" "any(.[]; .name == \"${to##*/}\")" "$SANDBOX/ls.json"
}

# A rename in place and a move to another directory keep the file's bytes.
inode="$(fs.ext4 "$img" stat /f | jq .inode)"
moved /f /f2
moved /f2 /b/f3
check "the moved file still reads its bytes" cmp -s "$SANDBOX/f" <(fs.ext4 "$img" read /b/f3)
fs.ext4 "$img" stat /b/f3 >"$SANDBOX/stat.json" 2>/dev/null
jq_check "the moved file is the same inode" ".inode == $inode and .nlink == 1" "$SANDBOX/stat.json"

# A directory moved to another parent takes its tree with it, its `..`
# names the new parent, and the old parent has one link fewer and the new
# one one more.
moved /a /b/a
check "the moved directory's tree still reads its bytes" cmp -s "$SANDBOX/f" <(fs.ext4 "$img" read /b/a/sub/y)
check "the moved directory's file still reads its bytes" cmp -s "$SANDBOX/f" <(fs.ext4 "$img" read /b/a/g)
b_inode="$(fs.ext4 "$img" stat /b | jq .inode)"
fs.ext4 "$img" stat /b/a/.. >"$SANDBOX/stat.json" 2>/dev/null
jq_check "the moved directory's .. is its new parent" ".inode == $b_inode" "$SANDBOX/stat.json"
fs.ext4 "$img" stat /b >"$SANDBOX/stat.json" 2>/dev/null
jq_check "the new parent has a link for the moved directory" ".nlink == 3" "$SANDBOX/stat.json"
fs.ext4 "$img" stat / >"$SANDBOX/stat.json" 2>/dev/null
jq_check "the root has one link fewer: b, empty, full, moving and lost+found" ".nlink == 7" "$SANDBOX/stat.json"

# --replace puts a file over a file, whose old content is gone, and a
# directory over an empty directory.
moved /b/f3 /old --replace
check "the replacing file reads its own bytes" cmp -s "$SANDBOX/f" <(fs.ext4 "$img" read /old)
moved /moving /empty --replace
check "the replacing directory brought its file" cmp -s "$SANDBOX/f" <(fs.ext4 "$img" read /empty/inner)
fs.ext4 "$img" ls / >"$SANDBOX/ls.json" 2>/dev/null
jq_check "the root has every name it should and no other" \
    '([.[].name] | sort) == (["b", "empty", "full", "lost+found", "old"] | sort)' "$SANDBOX/ls.json"

# A rename onto itself succeeds and changes nothing, as rename(2) does.
cp "$img" "$SANDBOX/before.img"
fs.ext4 "$img" mv --replace /old /old >"$SANDBOX/mv.json" 2>"$SANDBOX/mv.err"
rc=$?
check "mv --replace of a name onto itself exits 0 ($(cat "$SANDBOX/mv.err"))" test "$rc" -eq 0
jq_check "mv --replace of a name onto itself reports it moved" \
    '.path == "/old" and .moved == true' "$SANDBOX/mv.json"
check "mv --replace of a name onto itself left the image byte-identical" cmp -s "$img" "$SANDBOX/before.img"

fs.ext4 "$img" mv --text /old /older >"$SANDBOX/text.out" 2>/dev/null
check "mv --text says what it moved" test "$(cat "$SANDBOX/text.out")" = "moved /old to /older"

# Refusals: status 4 or 1 (state_code), a structured error naming why, nothing on stdout,
# and the image byte-identical. The reasons are the ones the kernel gives
# renameat2, RENAME_NOREPLACE unless --replace (tests/cli_mv_kernel.rs).
cp "$img" "$SANDBOX/before.img"
refused() {
    local why="$1"
    shift
    fs.ext4 "$img" mv "$@" >"$SANDBOX/x.out" 2>"$SANDBOX/x.err"
    local rc=$? want
    want="$(state_code "$why")"
    check "mv $* exits $want, not $rc" test "$rc" -eq "$want"
    check "mv $* prints nothing on stdout" test ! -s "$SANDBOX/x.out"
    jq_check "mv $* is refused: $why" ".code == $want and (.error | test(\"$why\"))" "$SANDBOX/x.err"
    check "mv $* left the image byte-identical" cmp -s "$img" "$SANDBOX/before.img"
}
long="$(printf 'n%.0s' $(seq 256))"
refused "not found" /missing /x
refused "not found" /older /missing/x
refused "not a directory" /older/x /y
refused "not a directory" /older/ /y
refused "not a directory" /older /y/
refused "already exists" /older /b/a/g
refused "already exists" /older /older
refused "already exists" /b /full
refused "is a directory" --replace /older /full
refused "not a directory" --replace /b /older
refused "directory not empty" --replace /empty /full
refused "invalid argument" /b /b/a/z
refused "invalid argument" --replace /b /b/a
refused "resource busy" /b/. /z
refused "resource busy" /b/.. /z
refused "resource busy" / /z
refused "resource busy" --replace /older /b/.
refused "name too long" /older "/$long"

fs.ext4 "$img" mv /older >"$SANDBOX/u.out" 2>"$SANDBOX/u.err"
check "mv with one path is a usage error" test $? -eq 2
check "mv with one path prints nothing on stdout" test ! -s "$SANDBOX/u.out"

# --offset is where the filesystem starts: a MiB of zeros before it is
# never touched.
off="$SANDBOX/offset.img"
{ head -c 1048576 /dev/zero; cat "$img"; } >"$off"
fs.ext4 --offset 1048576 "$off" mv /older /b/older >"$SANDBOX/o.json" 2>"$SANDBOX/o.err"
rc=$?
check "mv with --offset exits 0 ($(cat "$SANDBOX/o.err"))" test "$rc" -eq 0
fs.ext4 --offset 1048576 "$off" ls /b >"$SANDBOX/ls.json" 2>/dev/null
jq_check "mv with --offset moved the name" 'any(.[]; .name == "older")' "$SANDBOX/ls.json"
check "the MiB before --offset is still zeros" cmp -s <(head -c 1048576 "$off") <(head -c 1048576 /dev/zero)

# The volume is clean afterwards, by our checker and by the flag.
fsck.ext4 "$img" >"$SANDBOX/fsck.json" 2>/dev/null
check "fsck.ext4 after the moves exits 0" test $? -eq 0
check "the volume is not dirty after the moves" test "$(fs.ext4 "$img" get dirty --text)" = false
fsck.ext4 --offset 1048576 "$off" >/dev/null 2>&1
check "fsck.ext4 at --offset after the move exits 0" test $? -eq 0

finish
