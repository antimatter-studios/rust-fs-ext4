#!/usr/bin/env bash
#
# ci-guest-run.sh <executable> [args...] — cargo's target runner on a CI
# runner without KVM: run one test executable INSIDE the guest that
# `scripts/ci-qemu-runner.sh serve` booted beside this command.
#
#   CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER=scripts/ci-guest-run.sh  chore test
#   CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER=scripts/ci-guest-run.sh chore test:unit
#
# Cargo builds here, natively (or for the guest's architecture with its cross
# linker), and hands each test executable to this script instead of starting
# it. The executable then runs in the guest with FLTH_GUEST=1, where
# fs_ext4_test_support reaches e2fsprogs, the kernel and lwext4 directly, as
# it does in the harness VM. Nothing about the tiers changes: the same
# `chore` tasks, targets, output budgets and executed-test floors judge it.
#
# THE GUEST IS GIVEN THE PATHS THIS HOST KNOWS. Cargo compiles this
# checkout's path into every test (CARGO_MANIFEST_DIR, CARGO_BIN_EXE_*), so
# the first call copies the working tree (fixtures included, without .git,
# target/ and tmp/) and rust-fs-core to the same paths in the guest, and
# every call copies the executable it runs and the binaries beside it.
#
# Everything is read from FS_EXT4_GUEST_SSH, the ssh config whose host
# `guest` the serving script wrote; without it this is not a CI VM job, and
# it fails rather than running the test here.
set -euo pipefail

[ "$#" -gt 0 ] || { echo "usage: ci-guest-run.sh <executable> [args...]" >&2; exit 2; }
config="${FS_EXT4_GUEST_SSH:-}"
[ -n "$config" ] && [ -f "$config" ] || {
    echo "ci-guest-run.sh: FS_EXT4_GUEST_SSH names no guest; run this under 'scripts/ci-qemu-runner.sh serve'." >&2
    exit 1
}
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
state="$(dirname "$config")"
ssh_guest() { ssh -F "$config" guest "$@"; }

executable="$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1")"
shift

# One copy at a time, so two runners never interleave a transfer.
exec 9>"$state/ci-guest-run.lock"
flock 9

if [ ! -f "$state/ci-guest-tree" ]; then
    core="$(cd "$REPO/../rust-fs-core" && pwd -P)"
    for tree in "$REPO" "$core"; do
        tar --sparse -cf - -C "$tree" --exclude=./.git --exclude=./target --exclude=./tmp \
            --exclude=./.vm-share --exclude=./.vagrant . |
            ssh_guest "mkdir -p $(printf %q "$tree") && tar -xf - -C $(printf %q "$tree")"
    done
    touch "$state/ci-guest-tree"
fi

# The executable, and the binaries cargo built beside it (CARGO_BIN_EXE_*).
case "$(basename "$(dirname "$executable")")" in
    deps) profile="$(dirname "$(dirname "$executable")")" ;;
    *) profile="$(dirname "$executable")" ;;
esac
send=()
while IFS= read -r -d '' file; do
    signature="$(stat -c '%s %Y %i' "$file")"
    if ! grep -qxF "$file $signature" "$state/ci-guest-sent" 2>/dev/null; then
        send+=("${file#/}")
        printf '%s %s\n' "$file" "$signature" >>"$state/ci-guest-sent"
    fi
done < <(
    printf '%s\0' "$executable"
    find "$profile" -maxdepth 1 -type f -perm -u+x -print0
)
if [ "${#send[@]}" -gt 0 ]; then
    tar -cf - -C / "${send[@]}" | ssh_guest 'tar -xf - -C /'
fi
flock -u 9

# The test's environment, as cargo set it, less what names this host's
# toolchain; the guest's own toolchain is what a test that runs cargo gets.
remote_env=(FLTH_GUEST=1 HOME=/root
    PATH=/var/lib/fs-ext4-rust/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
    RUSTUP_HOME=/var/lib/fs-ext4-rust/rustup CARGO_HOME=/var/lib/fs-ext4-rust/cargo
    CARGO=/var/lib/fs-ext4-rust/cargo/bin/cargo)
while IFS= read -r name; do
    case "$name" in
        CARGO | CARGO_HOME | RUSTUP_* | FS_EXT4_GUEST_SSH | FS_EXT4_GUEST_ARCH) continue ;;
        CARGO_* | RUST_* | FS_EXT4_* | EXPECT_* | OUTPUT_BUDGET_* | TMPDIR | TERM | NO_COLOR | LANG | LC_ALL | TZ)
            remote_env+=("$name=${!name}") ;;
    esac
done < <(compgen -e)

scratch="${FS_EXT4_TEST_TMPDIR:-${TMPDIR:-$REPO/tmp}}"
command="mkdir -p $(printf %q "$scratch") && cd $(printf %q "$PWD") && exec env -i"
for word in "${remote_env[@]}" "$executable" "$@"; do
    command+=" $(printf %q "$word")"
done
status=0
ssh -n -F "$config" guest "$command" || status=$?
if [ "$status" = 255 ]; then
    echo "ci-guest-run.sh: ssh to the guest failed (255) running $executable" >&2
fi
exit "$status"
