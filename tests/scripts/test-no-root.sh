#!/usr/bin/env bash
# Nothing the tests run asks for root.
#
# The mounts and the oracle tools happen in the fs-linux-test-harness VM,
# so no test, fixture recipe or test script needs root on the machine
# running it. A `sudo` is how the tools come back onto the host without
# any Rust check seeing them: `sudo bash -c 'mkfs.ext4 ...; e2fsck ...'`
# spawns one program, and it is not an oracle tool (#287).
# tests/test_contract.rs is the thorough version of this over the Rust
# sources; this one also covers the shell, the fixture recipes and the
# examples.
#
# EVERY CHECK IS A CODE SHAPE, not a word. A comment that says "this used
# to run under sudo on the runner" is history worth keeping, so comment
# lines are not read.
#
# Two files may ask, and are not read: `scripts/tools.sh` installs the
# HOST's own packages with the system package manager, which is the one
# job root is for here; and tests/test_contract.rs spells the escalators
# out, because refusing them is its job.
#
#   bash tests/scripts/test-no-root.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SELF="$(basename "${BASH_SOURCE[0]}")"

# ripgrep is REQUIRED: no match is the passing case, so a missing `rg`
# would match nothing and report PASS.
if ! command -v rg >/dev/null 2>&1; then
    echo "FAIL  ripgrep (rg) is not installed; run 'chore tools'" >&2
    exit 1
fi

# An escalator in command position or as a string literal, `su` given a
# user or a login flag, and `"su"` as a literal.
ROOT='\b(sudo|doas|pkexec|run0)\b([\s"'"'"';)]|$)|(^|[\s;&|(`"'"'"'])su\s+(-|root\b)|"su"'
# A line whose first non-blank characters open a comment.
COMMENT='^[^:]+:[0-9]+:\s*(#|//|/\*|\*)'

# Every line under <root> (the repository, or a sandbox shaped like one)
# that asks for root.
scan() {
    local root="$1" dirs=() d
    for d in tests test-disks examples scripts; do
        [[ -d "$root/$d" ]] && dirs+=("$root/$d")
    done
    [[ ${#dirs[@]} -gt 0 ]] || { echo "$root: nothing to scan"; return; }
    rg -n -e "$ROOT" "${dirs[@]}" \
        --glob '!**/*.md' \
        --glob '!**/test_contract.rs' \
        --glob "!**/scripts/$SELF" \
        --glob '!**/scripts/tools.sh' \
        | rg -v "$COMMENT" || true
}

mkdir -p "$REPO/tmp"
SANDBOX="$(mktemp -d "$REPO/tmp/no-root.XXXXXX")"
trap 'rm -rf "$SANDBOX"' EXIT HUP INT TERM

fail() { echo "FAIL  $*" >&2; exit 1; }

# --- 1. The scan recognises the shapes it refuses. -----------------------
#
# Without this a pattern that matched nothing -- a typo, an rg that reads
# the flags differently -- would pass the real tree having checked nothing.
mkdir -p "$SANDBOX/tests/scripts" "$SANDBOX/test-disks" "$SANDBOX/scripts" "$SANDBOX/examples"
cat > "$SANDBOX/test-disks/recipe.sh" <<'EOF'
sudo mount -o loop "$img" /mnt
out=$(doas e2fsck -fn "$img")
su -c 'mkfs.ext4 -F x.img'
# this used to run under sudo mount on the runner
EOF
cat > "$SANDBOX/tests/escape.rs" <<'EOF'
// sudo is not needed here any more
let out = Command::new("sudo").args(["-n", "bash", "-c", "e2fsck -fn x.img"]);
let p = "/usr/bin/pkexec";
let q = "su";
EOF
cat > "$SANDBOX/scripts/helper.sh" <<'EOF'
run0 debugfs -R stats x.img
pseudo-sudo-free line: sudoers is a file, resume is a word
EOF
# Where root is allowed, and the name of this file, are not read.
printf 'sudo apt-get install -y ripgrep\n' > "$SANDBOX/scripts/tools.sh"
printf 'sudo true\n' > "$SANDBOX/tests/scripts/$SELF"

found="$(scan "$SANDBOX")"
expect=(
    "test-disks/recipe.sh:1:"
    "test-disks/recipe.sh:2:"
    "test-disks/recipe.sh:3:"
    "tests/escape.rs:2:"
    "tests/escape.rs:3:"
    "tests/escape.rs:4:"
    "scripts/helper.sh:1:"
)
for e in "${expect[@]}"; do
    grep -qF "$SANDBOX/$e" <<<"$found" || fail "the scan missed $e:"$'\n'"$found"
done
count="$(grep -c . <<<"$found")"
[[ "$count" -eq ${#expect[@]} ]] ||
    fail "the scan found $count lines, expected ${#expect[@]}:"$'\n'"$found"

# --- 2. The tree asks for root nowhere. -----------------------------------
found="$(scan "$REPO")"
if [[ -n "$found" ]]; then
    echo "FAIL  these ask for root; the mounts and the tools run in the guest, and nothing else here needs it:" >&2
    printf '%s\n' "$found" >&2
    exit 1
fi

echo "PASS  nothing the tests run asks for root"
