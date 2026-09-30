# fsck.ext4: fsck(8)'s exit statuses and a JSON report, on an image our own
# mkfs.ext4 makes. The damaged-image cases (a wrong group free count found,
# repaired and clean again; a destroyed root) need the image's checksums
# restamped, which is library work, so they live in tests/cli_fsck.rs and
# are held to e2fsck in tests/cli_fsck_oracle.rs.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/fsck.img"
mkfs.ext4 -q --text --size 64M "$img" >/dev/null 2>&1
check "mkfs.ext4 made the image" test -s "$img"

# A fresh image: status 0 and a clean report, whichever way it is asked.
for flags in "" "-n" "-fn" "-y" "-p"; do
    # shellcheck disable=SC2086  # the words are the point
    fsck.ext4 $flags "$img" >"$SANDBOX/fsck.json" 2>"$SANDBOX/fsck.err"
    rc=$?
    check "fsck.ext4 $flags on a fresh image exits 0 ($(cat "$SANDBOX/fsck.err"))" test "$rc" -eq 0
    jq_check "fsck.ext4 $flags reports clean" \
        '.fs == "ext4" and .clean == true and .exit == 0 and .found == 0 and (.findings | length) == 0' \
        "$SANDBOX/fsck.json"
done
jq_check "fsck.ext4 -n reports its scan counts as numbers" \
    '[.scanned.directories, .scanned.entries, .scanned.inodes] | all(type == "number")' "$SANDBOX/fsck.json"
check "fsck.ext4 --text says clean" grep -q ': clean (' <<<"$(fsck.ext4 --text "$img")"
check "rust-fs-ext4 fsck is the same program" test "$(rust-fs-ext4 fsck --text "$img")" = "$(fsck.ext4 --text "$img")"

# Cannot be opened: 8, a structured error, nothing on stdout.
fsck.ext4 "$SANDBOX/absent.img" >"$SANDBOX/absent.out" 2>"$SANDBOX/absent.err"
check "a missing image exits 8" test $? -eq 8
check "a missing image prints nothing on stdout" test ! -s "$SANDBOX/absent.out"
jq_check "a missing image is a structured error with code 8" '.code == 8' "$SANDBOX/absent.err"
head -c 2048 "$img" >"$SANDBOX/cut.img"
fsck.ext4 "$SANDBOX/cut.img" >"$SANDBOX/cut.out" 2>"$SANDBOX/cut.err"
check "a truncated image exits 8" test $? -eq 8
check "a truncated image prints nothing on stdout" test ! -s "$SANDBOX/cut.out"

# A wrong command line: 16, as fsck(8) documents, not the shared 2.
fsck.ext4 -n -y "$img" >"$SANDBOX/usage.out" 2>"$SANDBOX/usage.err"
check "-n with -y exits 16" test $? -eq 16
check "-n with -y prints nothing on stdout" test ! -s "$SANDBOX/usage.out"
jq_check "-n with -y is a structured error with code 16" '.code == 16' "$SANDBOX/usage.err"

finish
