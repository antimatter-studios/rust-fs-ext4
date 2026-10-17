# fs.ext4 ls --recursive, as installed: a nested tree our own tools made,
# listed depth first with names sorted as bytes, the same bytes from two
# runs, each bound reached (exit 1, the entries collected so far still on
# stdout, the bound and the stopping path named), the refusals, and an
# image left exactly as it was. The tree Linux made, the symlink to a
# directory and the damaged directory graphs are tests/cli_ls_recursive_kernel.rs.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/tree.img"
mkfs.ext4 -q --size 64M "$img" >/dev/null 2>&1
check "mkfs.ext4 made the image" test -s "$img"
for dir in /a /a/b /a/b/c; do
    fs.ext4 "$img" mkdir "$dir" >/dev/null 2>&1
    check "mkdir $dir exits 0" test $? -eq 0
done
printf deep | fs.ext4 "$img" write /a/b/c/deep >/dev/null 2>&1
check "write /a/b/c/deep exits 0" test $? -eq 0
printf ff | fs.ext4 "$img" write /a/f >/dev/null 2>&1
check "write /a/f exits 0" test $? -eq 0
printf z | fs.ext4 "$img" write /z >/dev/null 2>&1
check "write /z exits 0" test $? -eq 0
cp "$img" "$SANDBOX/before.img"

order='["/a","/a/b","/a/b/c","/a/b/c/deep","/a/f","/lost+found","/z"]'
fs.ext4 "$img" ls --recursive / >"$SANDBOX/tree.json" 2>"$SANDBOX/tree.err"
check "ls --recursive / exits 0 ($(cat "$SANDBOX/tree.err"))" test $? -eq 0
check "a walk that fits prints no error" test ! -s "$SANDBOX/tree.err"
jq_check "the walk is depth first, names sorted as bytes" "[.[].path] == $order" "$SANDBOX/tree.json"
jq_check "depth counts the steps down from the directory asked for" \
    '[.[].depth] == [1,2,3,4,2,1,1]' "$SANDBOX/tree.json"
jq_check "every entry is the ls entry plus path and depth" \
    'all(.[]; (.name|type)=="string" and (.type|type)=="string" and (.size|type)=="number"
        and (.mode|test("^[0-7]{4}$")) and (.mtime|type)=="number" and (.inode|type)=="number"
        and (.path|type)=="string" and (.depth|type)=="number" and (has("revisited")|not))' \
    "$SANDBOX/tree.json"
jq_check "each name is the last component of its path" \
    'all(.[]; (.path | split("/") | last) == .name)' "$SANDBOX/tree.json"
fs.ext4 "$img" ls / >"$SANDBOX/root.json" 2>/dev/null
check "the depth-1 entries are the ones plain ls / prints" \
    test "$(jq -c '[.[] | select(.depth == 1) | del(.path, .depth)]' "$SANDBOX/tree.json")" = "$(jq -c '[.[]]' "$SANDBOX/root.json")"
jq_check "plain ls is unchanged: no path, depth or revisited" \
    'length == 3 and all(.[]; (has("path") or has("depth") or has("revisited")) | not)' "$SANDBOX/root.json"

fs.ext4 "$img" ls -R / >"$SANDBOX/again.json" 2>/dev/null
check "-R exits 0" test $? -eq 0
check "two runs print the same bytes" cmp -s "$SANDBOX/tree.json" "$SANDBOX/again.json"
fs.ext4 "$img" ls -R --max-depth 4 --limit 7 / >"$SANDBOX/fit.json" 2>/dev/null
check "a tree that exactly fits both bounds exits 0" test $? -eq 0
check "a tree that exactly fits keeps every entry" cmp -s "$SANDBOX/tree.json" "$SANDBOX/fit.json"

# Each bound reached: status 1, the collected prefix on stdout as a whole
# JSON array, and the structured error naming the bound and the path
# where the walk stopped.
for spec in 'max-depth 3 3 /a/b/c/deep' 'limit 4 4 /a/f' 'max-depth 0 0 /a' 'limit 0 0 /a'; do
    read -r bound value count stopped <<<"$spec"
    fs.ext4 "$img" ls -R "--$bound" "$value" / >"$SANDBOX/partial.json" 2>"$SANDBOX/partial.err"
    check "--$bound $value exits 1" test $? -eq 1
    jq_check "--$bound $value keeps the $count entries collected" "length == $count" "$SANDBOX/partial.json"
    check "--$bound $value keeps exactly the walk's first $count entries" \
        test "$(jq -c . "$SANDBOX/partial.json")" = "$(jq -c ".[0:$count]" "$SANDBOX/tree.json")"
    jq_check "--$bound $value names the bound and $stopped" \
        ".code == 1 and (.error | contains(\"--$bound $value\")) and (.error | contains(\"$stopped\"))" \
        "$SANDBOX/partial.err"
done

fs.ext4 "$img" ls -R a//b/ >"$SANDBOX/sub.json" 2>/dev/null
check "a subtree with a relative, doubled path exits 0" test $? -eq 0
jq_check "a subtree's paths are from the volume's root, its depths from the subtree" \
    '[.[] | [.path, .depth]] == [["/a/b/c",1],["/a/b/c/deep",2]]' "$SANDBOX/sub.json"
fs.ext4 "$img" ls -R /z >"$SANDBOX/file.json" 2>/dev/null
check "a walk that starts at a file exits 0" test $? -eq 0
jq_check "a walk that starts at a file lists it alone, at depth 0" \
    'length == 1 and .[0].path == "/z" and .[0].depth == 0 and .[0].type == "file" and .[0].size == 1' \
    "$SANDBOX/file.json"
fs.ext4 "$img" ls -R --limit 0 /z >"$SANDBOX/file0.json" 2>"$SANDBOX/file0.err"
check "--limit 0 at a file exits 1" test $? -eq 1
jq_check "--limit 0 at a file keeps an empty array" 'length == 0' "$SANDBOX/file0.json"

fs.ext4 "$img" ls -R --text / >"$SANDBOX/tree.txt" 2>/dev/null
check "ls -R --text exits 0" test $? -eq 0
check "ls -R --text prints one line per entry" test "$(wc -l <"$SANDBOX/tree.txt")" -eq 7
check "ls -R --text ends each line with the full path" grep -q ' /a/b/c/deep$' "$SANDBOX/tree.txt"
fs.ext4 --text "$img" ls -R --json --limit 1 / >"$SANDBOX/format.json" 2>"$SANDBOX/format.err"
check "a bound reached after --text then --json exits 1" test $? -eq 1
jq_check "the partial report is in the format given last" 'length == 1' "$SANDBOX/format.json"
jq_check "the error is in the format given last" '.code == 1' "$SANDBOX/format.err"
fs.ext4 "$img" ls -R --text --limit 2 / >"$SANDBOX/partial.txt" 2>"$SANDBOX/partial-text.err"
check "a bound reached with --text exits 1" test $? -eq 1
check "a bound reached with --text keeps its lines" test "$(wc -l <"$SANDBOX/partial.txt")" -eq 2

# Refusals: status 2 or 1, nothing on stdout.
for args in '--max-depth 2' '--limit 2' '-R --limit nope' '-R --max-depth -1'; do
    # shellcheck disable=SC2086 # the words are the command line under test
    fs.ext4 "$img" ls $args / >"$SANDBOX/usage.out" 2>"$SANDBOX/usage.err"
    check "ls $args is a usage error" test $? -eq 2
    check "ls $args prints nothing on stdout" test ! -s "$SANDBOX/usage.out"
    jq_check "ls $args says so in the structured error" '.code == 2' "$SANDBOX/usage.err"
done
fs.ext4 "$img" ls -R /missing >"$SANDBOX/missing.out" 2>"$SANDBOX/missing.err"
check "ls -R of a missing path exits 1" test $? -eq 1
check "ls -R of a missing path prints nothing on stdout" test ! -s "$SANDBOX/missing.out"
jq_check "ls -R of a missing path says not found" '.code == 1 and (.error | test("not found"))' "$SANDBOX/missing.err"

{ head -c 1048576 /dev/zero; cat "$img"; } >"$SANDBOX/whole-disk.img"
fs.ext4 --offset 1048576 "$SANDBOX/whole-disk.img" ls -R / >"$SANDBOX/offset.json" 2>/dev/null
check "ls -R with --offset exits 0" test $? -eq 0
check "ls -R with --offset lists the same tree" cmp -s "$SANDBOX/tree.json" "$SANDBOX/offset.json"

check "ls -R left the image as it was" cmp -s "$img" "$SANDBOX/before.img"

finish
