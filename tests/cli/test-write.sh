# fs.ext4 write and mkdir, as installed: round trips compared with cmp at
# every block-size boundary, a file in a subdirectory, a replacement by a
# shorter file, typed ls entries on the populated image, refusals, mkdir
# --parents making a missing chain (and nothing, without writing, for a
# chain already there), and a clean fsck.ext4 afterwards.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/write.img"
mkfs.ext4 -q --text --size 64M "$img" >/dev/null 2>&1
check "mkfs.ext4 made the image" test -s "$img"

fs.ext4 "$img" mkdir /d >"$SANDBOX/mkdir.json" 2>"$SANDBOX/mkdir.err"
rc=$?
check "mkdir /d exits 0 ($(cat "$SANDBOX/mkdir.err"))" test "$rc" -eq 0
jq_check "mkdir reports the path and a numeric inode" '.path == "/d" and (.inode | type) == "number"' "$SANDBOX/mkdir.json"
fs.ext4 "$img" mkdir /d/e >/dev/null 2>&1
check "mkdir /d/e exits 0" test $? -eq 0

# Every size boundary a 4 KiB block has, a MiB of noise, and a deep file.
for size in 0 1 4095 4096 4097 1048576; do
    head -c "$size" /dev/urandom >"$SANDBOX/src.$size"
    fs.ext4 "$img" write "/f$size" <"$SANDBOX/src.$size" >"$SANDBOX/w.json" 2>"$SANDBOX/w.err"
    rc=$?
    check "write /f$size exits 0 ($(cat "$SANDBOX/w.err"))" test "$rc" -eq 0
    jq_check "write /f$size reports $size bytes, created" ".bytes == $size and .created == true" "$SANDBOX/w.json"
    fs.ext4 "$img" read "/f$size" >"$SANDBOX/back.$size" 2>/dev/null
    check "read /f$size matches what was written" cmp -s "$SANDBOX/src.$size" "$SANDBOX/back.$size"
done
head -c 5000 /dev/urandom >"$SANDBOX/deep"
fs.ext4 "$img" write /d/e/deep <"$SANDBOX/deep" >/dev/null 2>&1
check "write /d/e/deep exits 0" test $? -eq 0
check "read /d/e/deep matches" cmp -s "$SANDBOX/deep" <(fs.ext4 "$img" read /d/e/deep)

# Replaced by a shorter file: the old tail must not survive.
fs.ext4 "$img" write /f4097 <"$SANDBOX/src.1" >"$SANDBOX/r.json" 2>/dev/null
jq_check "replacing /f4097 reports 1 byte, not created" '.bytes == 1 and .created == false' "$SANDBOX/r.json"
check "read /f4097 is the shorter content" cmp -s "$SANDBOX/src.1" <(fs.ext4 "$img" read /f4097)

# ls on a populated image: every entry typed, the directory a directory.
fs.ext4 "$img" ls / >"$SANDBOX/ls.json" 2>/dev/null
jq_check "every ls entry has typed fields" \
    'length > 0 and all(.[]; (.name|type)=="string" and (.type|type)=="string" and (.size|type)=="number" and (.mode|test("^[0-7]{4}$")) and (.mtime|type)=="number" and (.inode|type)=="number")' \
    "$SANDBOX/ls.json"
fs.ext4 "$img" ls /d >"$SANDBOX/lsd.json" 2>/dev/null
jq_check "ls /d shows e as a directory" 'any(.[]; .name == "e" and .type == "dir")' "$SANDBOX/lsd.json"

# Refusals: status 1, a structured error, nothing on stdout, image unchanged.
cp "$img" "$SANDBOX/before.img"
fs.ext4 "$img" mkdir /d >"$SANDBOX/x.out" 2>"$SANDBOX/x.err"
check "mkdir of an existing path exits 1" test $? -eq 1
check "mkdir of an existing path prints nothing on stdout" test ! -s "$SANDBOX/x.out"
jq_check "mkdir of an existing path says so" '.code == 1 and (.error | test("already exists"))' "$SANDBOX/x.err"
echo x | fs.ext4 "$img" write /missing/f >"$SANDBOX/y.out" 2>"$SANDBOX/y.err"
check "write under a missing parent exits 1" test $? -eq 1
check "write under a missing parent prints nothing on stdout" test ! -s "$SANDBOX/y.out"
jq_check "write under a missing parent is a structured error" '.code == 1 and (.error | test("not found"))' "$SANDBOX/y.err"
check "the refusals left the image as it was" cmp -s "$img" "$SANDBOX/before.img"
fs.ext4 "$img" mkdir /m1/m2 >"$SANDBOX/x.out" 2>"$SANDBOX/x.err"
check "plain mkdir under a missing parent exits 1" test $? -eq 1
jq_check "plain mkdir makes no parent" '.code == 1 and (.error | test("not found"))' "$SANDBOX/x.err"
check "plain mkdir under a missing parent left the image as it was" cmp -s "$img" "$SANDBOX/before.img"

# mkdir --parents makes every missing directory on the way, mode 0755, and
# reports only those; one that is already there is passed through.
root_links="$(fs.ext4 "$img" stat / | jq .nlink)"
fs.ext4 "$img" mkdir --parents /p/q/r >"$SANDBOX/p.json" 2>"$SANDBOX/p.err"
rc=$?
check "mkdir --parents /p/q/r exits 0 ($(cat "$SANDBOX/p.err"))" test "$rc" -eq 0
jq_check "mkdir --parents reports the path and each directory it made" \
    '.path == "/p/q/r" and .created == ["/p", "/p/q", "/p/q/r"] and (keys | length) == 2' \
    "$SANDBOX/p.json"
for dir in "/p 3" "/p/q 3" "/p/q/r 2"; do
    read -r name links <<<"$dir"
    fs.ext4 "$img" stat "$name" >"$SANDBOX/stat.json" 2>/dev/null
    jq_check "$name is a directory, mode 0755, with $links links" \
        ".type == \"dir\" and .mode == \"0755\" and .nlink == $links" "$SANDBOX/stat.json"
done
check "the root has one link more, for /p" \
    test "$(fs.ext4 "$img" stat / | jq .nlink)" = "$((root_links + 1))"
fs.ext4 "$img" mkdir -p /d/e/s/t >"$SANDBOX/p.json" 2>/dev/null
jq_check "mkdir -p under an existing chain makes only what is missing" \
    '.path == "/d/e/s/t" and .created == ["/d/e/s", "/d/e/s/t"]' "$SANDBOX/p.json"
fs.ext4 "$img" mkdir -p /u/../v >"$SANDBOX/p.json" 2>/dev/null
jq_check "mkdir -p makes a directory that .. then leaves" \
    '.created == ["/u", "/u/../v"]' "$SANDBOX/p.json"
for name in /u /v; do
    check "$name was made" test "$(fs.ext4 "$img" stat "$name" 2>/dev/null | jq -r .type)" = dir
done
fs.ext4 "$img" mkdir -p --text /w/x >"$SANDBOX/p.txt" 2>/dev/null
check "mkdir -p --text names each directory made" \
    test "$(cat "$SANDBOX/p.txt")" = "$(printf 'created /w\ncreated /w/x')"

# A chain that is already there is success, and nothing is written.
cp "$img" "$SANDBOX/chain.img"
for p in /p/q/r /p/q/r/ /p/./q/.. /; do
    fs.ext4 "$img" mkdir -p "$p" >"$SANDBOX/p.json" 2>"$SANDBOX/p.err"
    rc=$?
    check "mkdir -p $p, already there, exits 0 ($(cat "$SANDBOX/p.err"))" test "$rc" -eq 0
    jq_check "mkdir -p $p reports nothing made" ".path == \"$p\" and .created == []" "$SANDBOX/p.json"
done
fs.ext4 "$img" mkdir -p --text /p/q >"$SANDBOX/p.txt" 2>/dev/null
check "mkdir -p --text says when nothing was made" \
    test "$(cat "$SANDBOX/p.txt")" = "/p/q exists; nothing created"
check "a chain already there was not written to" cmp -s "$img" "$SANDBOX/chain.img"

# Refusals: anything but a directory on the way, a name too long or an
# empty path is refused before anything is made, so no partial chain is
# left behind and the image is byte-identical. A symlink is never followed.
fs.ext4 "$img" ln -s /d /dl >/dev/null 2>&1
cp "$img" "$SANDBOX/before-p.img"
long="$(printf 'n%.0s' $(seq 256))"
refused() {
    local why="$1" p="$2"
    fs.ext4 "$img" mkdir -p "$p" >"$SANDBOX/x.out" 2>"$SANDBOX/x.err"
    local rc=$?
    check "mkdir -p ${p:0:40} exits 1, not $rc" test "$rc" -eq 1
    check "mkdir -p ${p:0:40} prints nothing on stdout" test ! -s "$SANDBOX/x.out"
    jq_check "mkdir -p ${p:0:40} is refused: $why" ".code == 1 and (.error | test(\"$why\"))" "$SANDBOX/x.err"
    check "mkdir -p ${p:0:40} left the image byte-identical" cmp -s "$img" "$SANDBOX/before-p.img"
}
refused "already exists" /f0
refused "already exists" /f0/
refused "not a directory" /f0/x
refused "not a directory" /f0/x/y
refused "not a directory" /new/../f0/x
refused "not a directory" /d/e/deep/x
refused "name too long" "/n1/n2/$long"
refused "not a directory" "/f0/$long"
refused "already exists" /dl
refused "not a directory" /dl/x
refused "not found" ""

# --offset is where the filesystem starts: a MiB of zeros before it is
# never touched.
off="$SANDBOX/offset.img"
{ head -c 1048576 /dev/zero; cat "$img"; } >"$off"
fs.ext4 --offset 1048576 "$off" mkdir -p /o1/o2 >"$SANDBOX/o.json" 2>"$SANDBOX/o.err"
rc=$?
check "mkdir -p with --offset exits 0 ($(cat "$SANDBOX/o.err"))" test "$rc" -eq 0
check "mkdir -p with --offset made the chain" \
    test "$(fs.ext4 --offset 1048576 "$off" stat /o1/o2 2>/dev/null | jq -r .type)" = dir
check "the MiB before --offset is still zeros" cmp -s <(head -c 1048576 "$off") <(head -c 1048576 /dev/zero)
fsck.ext4 --offset 1048576 "$off" >/dev/null 2>&1
check "fsck.ext4 at --offset after mkdir -p exits 0" test $? -eq 0

# The volume is clean afterwards, by our checker and by the flag.
fsck.ext4 "$img" >"$SANDBOX/fsck.json" 2>/dev/null
check "fsck.ext4 after the writes exits 0" test $? -eq 0
check "the volume is not dirty after the writes" test "$(fs.ext4 "$img" get dirty --text)" = false

finish
