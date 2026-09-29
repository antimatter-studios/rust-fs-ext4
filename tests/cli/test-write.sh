# fs.ext4 write and mkdir, as installed: round trips compared with cmp at
# every block-size boundary, a file in a subdirectory, a replacement by a
# shorter file, typed ls entries on the populated image, refusals, and a
# clean fsck.ext4 afterwards.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/write.img"
mkfs.ext4 -q --text --size 64M "$img" >/dev/null 2>&1
check "mkfs.ext4 made the image" test -s "$img"

fs.ext4 "$img" mkdir /d >"$SANDBOX/mkdir.json" 2>"$SANDBOX/mkdir.err"
check "mkdir /d exits 0 ($(cat "$SANDBOX/mkdir.err"))" test $? -eq 0
jq_check "mkdir reports the path and a numeric inode" '.path == "/d" and (.inode | type) == "number"' "$SANDBOX/mkdir.json"
fs.ext4 "$img" mkdir /d/e >/dev/null 2>&1
check "mkdir /d/e exits 0" test $? -eq 0

# Every size boundary a 4 KiB block has, a MiB of noise, and a deep file.
for size in 0 1 4095 4096 4097 1048576; do
    head -c "$size" /dev/urandom >"$SANDBOX/src.$size"
    fs.ext4 "$img" write "/f$size" <"$SANDBOX/src.$size" >"$SANDBOX/w.json" 2>"$SANDBOX/w.err"
    check "write /f$size exits 0 ($(cat "$SANDBOX/w.err"))" test $? -eq 0
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

# The volume is clean afterwards, by our checker and by the flag.
fsck.ext4 "$img" >"$SANDBOX/fsck.json" 2>/dev/null
check "fsck.ext4 after the writes exits 0" test $? -eq 0
check "the volume is not dirty after the writes" test "$(fs.ext4 "$img" get dirty --text)" = false

finish
