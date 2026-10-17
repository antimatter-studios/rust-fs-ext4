# fs.ext4 rm and rmdir, as installed: a file and an empty directory that
# write and mkdir made are removed and gone from ls, the blocks a removed
# file held are free again, --offset reaches a filesystem inside a larger
# image and nothing before it, every refusal is a structured error with
# nothing on stdout that leaves the image byte-identical, and the volume is
# clean afterwards by fsck.ext4 and by the dirty flag.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/rm.img"
mkfs.ext4 -q --text --size 64M "$img" >/dev/null 2>&1
check "mkfs.ext4 made the image" test -s "$img"

fs.ext4 "$img" mkdir /d >/dev/null 2>&1
fs.ext4 "$img" mkdir /d/e >/dev/null 2>&1
fs.ext4 "$img" mkdir /empty >/dev/null 2>&1
head -c 5000 /dev/urandom >"$SANDBOX/f"
fs.ext4 "$img" write /f <"$SANDBOX/f" >/dev/null 2>&1
fs.ext4 "$img" write /gone <"$SANDBOX/f" >/dev/null 2>&1
fs.ext4 "$img" write /d/kept <"$SANDBOX/f" >/dev/null 2>&1
check "the files to remove were written" cmp -s "$SANDBOX/f" <(fs.ext4 "$img" read /gone)

# removed PATH VERB DIR: VERB PATH exits 0, reports {"path", "removed": true},
# and DIR's listing no longer has the name.
removed() {
    local path="$1" verb="$2" dir="$3" name="${1##*/}"
    fs.ext4 "$img" "$verb" "$path" >"$SANDBOX/rm.json" 2>"$SANDBOX/rm.err"
    local rc=$?
    check "$verb $path exits 0 ($(cat "$SANDBOX/rm.err"))" test "$rc" -eq 0
    jq_check "$verb $path reports the path removed" \
        ".path == \"$path\" and .removed == true and (keys | length) == 2" "$SANDBOX/rm.json"
    fs.ext4 "$img" ls "$dir" >"$SANDBOX/ls.json" 2>/dev/null
    jq_check "ls $dir no longer has $name after $verb" "all(.[]; .name != \"$name\")" "$SANDBOX/ls.json"
}

removed /gone rm /
removed /empty rmdir /
removed /d/kept rm /d
fs.ext4 "$img" ls / >"$SANDBOX/ls.json" 2>/dev/null
jq_check "the names beside the removed ones are still there" \
    '([.[].name] | sort) == (["d", "f", "lost+found"] | sort)' "$SANDBOX/ls.json"
check "the file beside the removed one still reads its bytes" cmp -s "$SANDBOX/f" <(fs.ext4 "$img" read /f)

# A removed file's blocks are free again: a MiB written and removed leaves
# free_bytes where it was.
free_before="$(fs.ext4 "$img" get free_bytes --text)"
head -c 1048576 /dev/urandom | fs.ext4 "$img" write /big >/dev/null 2>&1
check "writing a MiB took space" test "$(fs.ext4 "$img" get free_bytes --text)" -lt "$free_before"
fs.ext4 "$img" rm --text /big >"$SANDBOX/text.out" 2>/dev/null
check "rm --text says what it removed" test "$(cat "$SANDBOX/text.out")" = "removed /big"
check "removing the MiB gave its blocks back" test "$(fs.ext4 "$img" get free_bytes --text)" -eq "$free_before"

# Refusals: status 1, a structured error naming why, nothing on stdout,
# and the image byte-identical.
cp "$img" "$SANDBOX/before.img"
refused() {
    local why="$1"
    shift
    fs.ext4 "$img" "$@" >"$SANDBOX/x.out" 2>"$SANDBOX/x.err"
    local rc=$?
    check "$* exits 1, not $rc" test "$rc" -eq 1
    check "$* prints nothing on stdout" test ! -s "$SANDBOX/x.out"
    jq_check "$* is refused: $why" ".code == 1 and (.error | test(\"$why\"))" "$SANDBOX/x.err"
    check "$* left the image byte-identical" cmp -s "$img" "$SANDBOX/before.img"
}
refused "not found" rm /missing
refused "not found" rmdir /missing
refused "not found" rm /missing/f
refused "is a directory" rm /d
refused "is a directory" rm /
refused "not a directory" rmdir /f
refused "not a directory" rm /f/
refused "not a directory" rm /f/x
refused "directory not empty" rmdir /d
refused "directory not empty" rmdir /d/..
refused "invalid argument" rmdir /d/.
refused "root directory" rmdir /

fs.ext4 "$img" rm >"$SANDBOX/u.out" 2>"$SANDBOX/u.err"
check "rm without a path is a usage error" test $? -eq 2
check "rm without a path prints nothing on stdout" test ! -s "$SANDBOX/u.out"

# --offset is where the filesystem starts: a MiB of zeros before it is
# never touched.
off="$SANDBOX/offset.img"
{ head -c 1048576 /dev/zero; cat "$img"; } >"$off"
fs.ext4 --offset 1048576 "$off" rm /f >"$SANDBOX/o.json" 2>"$SANDBOX/o.err"
rc=$?
check "rm with --offset exits 0 ($(cat "$SANDBOX/o.err"))" test "$rc" -eq 0
fs.ext4 --offset 1048576 "$off" ls / >"$SANDBOX/ls.json" 2>/dev/null
jq_check "rm with --offset removed the name" 'all(.[]; .name != "f")' "$SANDBOX/ls.json"
check "the MiB before --offset is still zeros" cmp -s <(head -c 1048576 "$off") <(head -c 1048576 /dev/zero)

# The volume is clean afterwards, by our checker and by the flag.
fsck.ext4 "$img" >"$SANDBOX/fsck.json" 2>/dev/null
check "fsck.ext4 after the removals exits 0" test $? -eq 0
check "the volume is not dirty after the removals" test "$(fs.ext4 "$img" get dirty --text)" = false
fsck.ext4 --offset 1048576 "$off" >/dev/null 2>&1
check "fsck.ext4 at --offset after the removal exits 0" test $? -eq 0

finish
