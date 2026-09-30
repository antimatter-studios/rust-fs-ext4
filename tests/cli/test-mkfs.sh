# mkfs.ext4: formats an image it creates, reports it as JSON, and fails
# with a structured error on stderr and nothing on stdout.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/cli.img"
mkfs.ext4 --size 64M --label CLITEST "$img" >"$SANDBOX/mkfs.json" 2>"$SANDBOX/mkfs.err"
rc=$?
check "mkfs.ext4 --size 64M --label CLITEST exits 0 ($(cat "$SANDBOX/mkfs.err"))" test "$rc" -eq 0
jq_check "the report is ext4 and formatted" '.fs == "ext4" and .formatted == true and .dry_run == false' "$SANDBOX/mkfs.json"
jq_check "the report carries the label" '.label == "CLITEST"' "$SANDBOX/mkfs.json"
jq_check "the report's sizes are numbers" \
    '[.block_size, .total_bytes, .total_blocks, .free_blocks, .total_inodes, .free_inodes, .device_bytes] | all(type == "number")' \
    "$SANDBOX/mkfs.json"
jq_check "the image is the size asked for" '.device_bytes == 67108864 and .total_bytes == 67108864' "$SANDBOX/mkfs.json"
jq_check "the UUID is 8-4-4-4-12 hex" '.uuid | test("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")' "$SANDBOX/mkfs.json"
check "the image exists at 64 MiB" test "$(wc -c <"$img" | tr -d ' ')" = 67108864

# The repository-named form is the same program.
rust-fs-ext4 mkfs -n "$img" >"$SANDBOX/repo.json" 2>/dev/null
check "rust-fs-ext4 mkfs -n exits 0" test $? -eq 0
jq_check "rust-fs-ext4 mkfs -n reports a dry run" '.dry_run == true and .formatted == false' "$SANDBOX/repo.json"

# --text: nothing on stdout, as the tool always printed.
mkfs.ext4 --text -q "$img" >"$SANDBOX/text.out" 2>"$SANDBOX/text.err"
check "mkfs.ext4 --text -q exits 0" test $? -eq 0
check "mkfs.ext4 --text -q prints nothing on stdout" test ! -s "$SANDBOX/text.out"
check "mkfs.ext4 --text -q prints nothing on stderr" test ! -s "$SANDBOX/text.err"

# A wrong command line: status 2, a JSON error on stderr, empty stdout.
mkfs.ext4 --no-such-flag "$img" >"$SANDBOX/usage.out" 2>"$SANDBOX/usage.err"
check "an unknown flag exits 2" test $? -eq 2
check "an unknown flag prints nothing on stdout" test ! -s "$SANDBOX/usage.out"
jq_check "an unknown flag is a structured error" '.code == 2 and (.error | test("no-such-flag"))' "$SANDBOX/usage.err"

# A failed run: status 1, the same shape.
mkfs.ext4 "$SANDBOX/absent.img" >"$SANDBOX/fail.out" 2>"$SANDBOX/fail.err"
check "a missing target exits 1" test $? -eq 1
check "a missing target prints nothing on stdout" test ! -s "$SANDBOX/fail.out"
jq_check "a missing target is a structured error" '.code == 1 and (.error | type) == "string"' "$SANDBOX/fail.err"

# The standard formatter's ignored flags are still accepted.
mkfs.ext4 -n -q -m 1 -c -T small -E lazy_itable_init=0 "$img" >/dev/null 2>&1
check "the accepted-and-ignored flags still parse" test $? -eq 0

finish
