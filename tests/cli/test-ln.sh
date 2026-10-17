# fs.ext4 ln, as installed: a hard link gives a file a second name, both
# names reading the same bytes from one inode whose link count rose, and a
# symlink is made relative or absolute, fast (inline) or slow (in a block),
# with its target the text as given. --offset reaches a filesystem inside a
# larger image and nothing before it, every refusal is a structured error
# with nothing on stdout that leaves the image byte-identical, and the
# volume is clean afterwards by fsck.ext4 and by the dirty flag.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/ln.img"
mkfs.ext4 -q -b 4096 --size 64M "$img" >/dev/null 2>&1
check "mkfs.ext4 made the image" test -s "$img"

for dir in /d /d/sub /e; do
    fs.ext4 "$img" mkdir "$dir" >/dev/null 2>&1
done
head -c 5000 /dev/urandom >"$SANDBOX/f"
fs.ext4 "$img" write /f <"$SANDBOX/f" >/dev/null 2>&1
fs.ext4 "$img" write /g <"$SANDBOX/f" >/dev/null 2>&1
check "the files to link were written" cmp -s "$SANDBOX/f" <(fs.ext4 "$img" read /f)

# linked TARGET PATH [OPTION]: ln exits 0 and reports {"path": PATH,
# "linked": true, "inode": N}, and PATH's directory lists the new name.
# The report is left in $SANDBOX/ln.json.
linked() {
    local from="$1" to="$2"
    shift 2
    fs.ext4 "$img" ln "$@" "$from" "$to" >"$SANDBOX/ln.json" 2>"$SANDBOX/ln.err"
    local rc=$?
    check "ln $* ${from:0:40} $to exits 0 ($(cat "$SANDBOX/ln.err"))" test "$rc" -eq 0
    jq_check "ln ${from:0:40} $to reports the path linked, with its inode" \
        ".path == \"$to\" and .linked == true and (.inode | type) == \"number\" and (keys | length) == 3" \
        "$SANDBOX/ln.json"
    fs.ext4 "$img" ls "${to%/*}/" >"$SANDBOX/ls.json" 2>/dev/null
    jq_check "${to%/*}/ has ${to##*/}" "any(.[]; .name == \"${to##*/}\")" "$SANDBOX/ls.json"
}

# A hard link: the new name is the file's inode, both names read the same
# bytes, and the link count is 2 by either name.
inode="$(fs.ext4 "$img" stat /f | jq .inode)"
linked /f /d/f2
jq_check "the hard link reports the file's inode" ".inode == $inode" "$SANDBOX/ln.json"
check "the new name reads the file's bytes" cmp -s "$SANDBOX/f" <(fs.ext4 "$img" read /d/f2)
for name in /f /d/f2; do
    fs.ext4 "$img" stat "$name" >"$SANDBOX/stat.json" 2>/dev/null
    jq_check "$name is the file, with two links" \
        ".inode == $inode and .type == \"file\" and .nlink == 2" "$SANDBOX/stat.json"
done

# Symlinks: the target is the text as given, never resolved, so a relative
# one, an absolute one and one naming nothing are all made. Under 60 bytes
# it is held in the inode and allocates nothing; from 60 it has a block of
# its own.
slow="/$(printf 's%.0s' $(seq 199))"
longest="$(printf 'a%.0s' $(seq 4095))"
for link in "../f /d/rel 0" "/d/sub /abs 0" "nowhere /dangle 0" "$slow /slow 4096"; do
    read -r text name allocated <<<"$link"
    linked "$text" "$name" -s
    new="$(jq .inode "$SANDBOX/ln.json")"
    fs.ext4 "$img" stat "$name" >"$SANDBOX/stat.json" 2>/dev/null
    jq_check "$name is a new symlink to ${text:0:40}, allocating $allocated bytes" \
        ".inode == $new and .inode != $inode and .type == \"symlink\" and .nlink == 1 \
         and .target == \"$text\" and .size == ${#text} and .allocated_bytes == $allocated" \
        "$SANDBOX/stat.json"
done
linked "$longest" /longest --symbolic
fs.ext4 "$img" stat /longest >"$SANDBOX/stat.json" 2>/dev/null
jq_check "a target of 4095 bytes, one less than a 4096-byte block, is kept whole" \
    ".type == \"symlink\" and .size == 4095 and .target == \"$longest\"" "$SANDBOX/stat.json"
check "the file the relative symlink names still has two links" \
    test "$(fs.ext4 "$img" stat /f | jq .nlink)" = 2

# A hard link to a symlink names the symlink itself, which is not followed.
symlink="$(fs.ext4 "$img" stat /d/rel | jq .inode)"
linked /d/rel /rel2
fs.ext4 "$img" stat /rel2 >"$SANDBOX/stat.json" 2>/dev/null
jq_check "a hard link to a symlink is the symlink, with two links" \
    ".inode == $symlink and .type == \"symlink\" and .nlink == 2 and .target == \"../f\"" \
    "$SANDBOX/stat.json"

fs.ext4 "$img" ln --text /g /g2 >"$SANDBOX/text.out" 2>/dev/null
check "ln --text says what it linked" test "$(cat "$SANDBOX/text.out")" = "linked /g2 to /g"
fs.ext4 "$img" ln --text -s ../g /d/g >"$SANDBOX/text.out" 2>/dev/null
check "ln -s --text says what it made" test "$(cat "$SANDBOX/text.out")" = "linked /d/g -> ../g"

# Refusals: status 1, a structured error naming why, nothing on stdout,
# and the image byte-identical. The reasons are the ones the kernel gives
# linkat and symlinkat (tests/cli_ln_kernel.rs).
cp "$img" "$SANDBOX/before.img"
refused() {
    local why="$1"
    shift
    fs.ext4 "$img" ln "$@" >"$SANDBOX/x.out" 2>"$SANDBOX/x.err"
    local rc=$?
    local args="$*"
    args="${args:0:60}"
    check "ln $args exits 1, not $rc" test "$rc" -eq 1
    check "ln $args prints nothing on stdout" test ! -s "$SANDBOX/x.out"
    jq_check "ln $args is refused: $why" ".code == 1 and (.error | test(\"$why\"))" "$SANDBOX/x.err"
    check "ln $args left the image byte-identical" cmp -s "$img" "$SANDBOX/before.img"
}
long="$(printf 'n%.0s' $(seq 256))"
refused "not found" /missing /x
refused "not found" /f /missing/x
refused "not a directory" /f/x /y
refused "not a directory" /f/ /y
refused "not a directory" /f /f/x
refused "already exists" /f /g
refused "already exists" /f /d
refused "already exists" /f /dangle
refused "already exists" /f /d/.
refused "already exists" /f /
refused "not found" /f /y/
refused "not permitted" /d /x
refused "not permitted" / /x
refused "not permitted" /d/sub/.. /x
refused "already exists" /d /g
refused "name too long" /f "/$long"
refused "already exists" -s t /g
refused "already exists" -s t /dangle
refused "already exists" -s t /d/..
refused "not found" -s t /missing/x
refused "not a directory" -s t /f/x
refused "not found" -s t /y/
refused "not found" -s "" /x
refused "name too long" -s t "/$long"
refused "name too long" -s "${longest}a" /x
refused "name too long" -s "${longest}a" /g

fs.ext4 "$img" ln /f >"$SANDBOX/u.out" 2>"$SANDBOX/u.err"
check "ln with one path is a usage error" test $? -eq 2
check "ln with one path prints nothing on stdout" test ! -s "$SANDBOX/u.out"

# A 1 KiB block holds a target of 1023 bytes and no more.
small="$SANDBOX/small.img"
mkfs.ext4 -q -b 1024 --size 8M "$small" >/dev/null 2>&1
fs.ext4 "$small" ln -s "${longest:0:1023}" /most >"$SANDBOX/s.json" 2>"$SANDBOX/s.err"
rc=$?
check "a 1023-byte target on a 1 KiB-block volume exits 0 ($(cat "$SANDBOX/s.err"))" test "$rc" -eq 0
fs.ext4 "$small" stat /most >"$SANDBOX/stat.json" 2>/dev/null
jq_check "the 1023-byte target is kept whole" ".size == 1023 and .target == \"${longest:0:1023}\"" \
    "$SANDBOX/stat.json"
cp "$small" "$SANDBOX/small-before.img"
fs.ext4 "$small" ln -s "${longest:0:1024}" /over >"$SANDBOX/x.out" 2>"$SANDBOX/x.err"
rc=$?
check "a 1024-byte target on a 1 KiB-block volume exits 1, not $rc" test "$rc" -eq 1
jq_check "a 1024-byte target on a 1 KiB-block volume is refused: name too long" \
    '.code == 1 and (.error | test("name too long"))' "$SANDBOX/x.err"
check "the refused 1024-byte target left the image byte-identical" \
    cmp -s "$small" "$SANDBOX/small-before.img"
fsck.ext4 "$small" >/dev/null 2>&1
check "fsck.ext4 after the 1 KiB-block symlink exits 0" test $? -eq 0

# --offset is where the filesystem starts: a MiB of zeros before it is
# never touched.
off="$SANDBOX/offset.img"
{ head -c 1048576 /dev/zero; cat "$img"; } >"$off"
fs.ext4 --offset 1048576 "$off" ln /f /e/f >"$SANDBOX/o.json" 2>"$SANDBOX/o.err"
rc=$?
check "ln with --offset exits 0 ($(cat "$SANDBOX/o.err"))" test "$rc" -eq 0
fs.ext4 --offset 1048576 "$off" stat /e/f >"$SANDBOX/stat.json" 2>/dev/null
jq_check "ln with --offset linked the file" ".inode == $inode and .nlink == 3" "$SANDBOX/stat.json"
check "the MiB before --offset is still zeros" cmp -s <(head -c 1048576 "$off") <(head -c 1048576 /dev/zero)

# The volume is clean afterwards, by our checker and by the flag.
fsck.ext4 "$img" >"$SANDBOX/fsck.json" 2>/dev/null
check "fsck.ext4 after the links exits 0" test $? -eq 0
check "the volume is not dirty after the links" test "$(fs.ext4 "$img" get dirty --text)" = false
fsck.ext4 --offset 1048576 "$off" >/dev/null 2>&1
check "fsck.ext4 at --offset after the link exits 0" test $? -eq 0

finish
