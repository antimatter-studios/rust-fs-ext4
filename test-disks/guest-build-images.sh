#!/usr/bin/env bash
#
# guest-build-images.sh <output-dir> <image>... — GUEST side: runs as root
# inside the fs-linux-test-harness VM (Debian), started by
# test-disks/build-fixtures.sh through `vm.sh run`.
#
# These fixtures need the real kernel: each one is formatted, then
# LOOP-MOUNTED and populated through the in-kernel ext4 driver (files,
# htree directories, sparse files, xattrs, ACLs, inline data, a GPT
# partition), which no host tool can do and macOS cannot do at all.
# The tooling (e2fsprogs, attr, acl, fdisk) is installed by
# scripts/vm-setup.sh.
#
# Every image is built on the guest's own disk and copied to
# <output-dir> (on the share) only once it is complete and unmounted, so
# a failure half way never leaves a truncated image where the host picks
# fixtures up. Test expectations in tests/*.rs and the
# test-disks/*.meta.txt files depend on the exact content written here.
#
# DETERMINISM. Every mkfs pins the UUID (-U) and the directory hash seed
# (-E hash_seed), so the layout of every directory and every checksum
# seed is the same on every build. What the kernel stamps at mount time
# (mount/write times) still differs, so two builds are identical in
# structure and content rather than byte for byte.
set -euo pipefail

[ $# -ge 2 ] || { echo "usage: guest-build-images.sh <output-dir> <image>..." >&2; exit 2; }
OUTPUT_DIR="$1"
shift

WORK="$(mktemp -d /var/tmp/ext4-fixtures.XXXXXX)"
MNT="$WORK/mnt"
mkdir -p "$MNT" "$OUTPUT_DIR"
LOOP=""
cleanup() {
    mountpoint -q "$MNT" && umount "$MNT" || true
    [ -n "$LOOP" ] && losetup -d "$LOOP" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

HASH_SEED=a1b2c3d4-e5f6-7890-abcd-ef1234567890

# mkfs_det <uuid-suffix> <mkfs.ext4 args...> — mkfs.ext4 with the UUID
# and hash seed pinned. One UUID per image (the suffix), so no two
# fixtures share a checksum seed by accident.
mkfs_det() {
    local suffix="$1"
    shift
    mkfs.ext4 -q -F -U "e4f1c0de-0000-4000-8000-0000000000$suffix" "$@"
}

# The hash seed goes through -E, and -E takes one comma-separated list,
# so an image that needs other extended options passes them here.
EXT_DET="hash_seed=$HASH_SEED"

# finish <image> [check] — unmount, then copy the finished image out,
# keeping it sparse (largedir is 192 MiB of mostly nothing).
#
# A <check> function, when given, reads the unmounted image before it is
# copied, so an image that fails it never reaches <output-dir>.
finish() {
    local img="$1" check="${2:-}"
    sync
    umount "$MNT"
    if [ -n "$check" ]; then
        "$check" "$WORK/$img"
    fi
    cp --sparse=always "$WORK/$img" "$OUTPUT_DIR/$img.partial"
    mv -f "$OUTPUT_DIR/$img.partial" "$OUTPUT_DIR/$img"
    rm -f "$WORK/$img"
    echo "[guest] built $img"
}

build_basic() {
    local img=ext4-basic.img
    truncate -s 16M "$WORK/$img"
    mkfs_det 01 -b 4096 -O has_journal,ext_attr,dir_index,filetype,extent,64bit,flex_bg,sparse_super,metadata_csum \
        -E "$EXT_DET" -L testvolume "$WORK/$img"
    mount -t ext4 -o loop "$WORK/$img" "$MNT"
    printf 'hello from ext4\n' > "$MNT/test.txt"
    mkdir -p "$MNT/subdir"
    # /subdir needs at least one entry so rmdir-on-nonempty-dir tests
    # actually hit ENOTEMPTY.
    echo 'nested' > "$MNT/subdir/nested.txt"
    ln -s test.txt "$MNT/link.txt"
    finish "$img"
}

build_htree() {
    local img=ext4-htree.img
    truncate -s 16M "$WORK/$img"
    mkfs_det 02 -b 4096 -O has_journal,ext_attr,dir_index,filetype,extent,64bit,flex_bg,sparse_super,large_file,huge_file,uninit_bg,metadata_csum \
        -E "$EXT_DET" -L htree-vol "$WORK/$img"
    mount -t ext4 -o loop "$WORK/$img" "$MNT"
    mkdir -p "$MNT/bigdir"
    local i
    for i in $(seq 1 256); do
        printf 'content of file %03d\n' "$i" > "$MNT/bigdir/file_$i.txt"
    done
    echo 'small file content' > "$MNT/small.txt"
    finish "$img"
}

build_csum_seed() {
    local img=ext4-csum-seed.img
    truncate -s 16M "$WORK/$img"
    mkfs_det 03 -b 4096 -O has_journal,extent,64bit,flex_bg,metadata_csum,metadata_csum_seed \
        -E "$EXT_DET" -L csum-seed-vol "$WORK/$img"
    mount -t ext4 -o loop "$WORK/$img" "$MNT"
    echo 'pi-style file' > "$MNT/hello.txt"
    mkdir -p "$MNT/etc"
    echo 'fake fstab' > "$MNT/etc/fstab"
    finish "$img"
}

build_no_csum() {
    local img=ext4-no-csum.img
    truncate -s 8M "$WORK/$img"
    mkfs_det 04 -b 4096 -O ^metadata_csum,extent,64bit,filetype,dir_index,sparse_super \
        -E "$EXT_DET" -L no-csum-vol "$WORK/$img"
    mount -t ext4 -o loop "$WORK/$img" "$MNT"
    echo 'no checksum here' > "$MNT/file.txt"
    finish "$img"
}

build_deep_extents() {
    local img=ext4-deep-extents.img
    truncate -s 64M "$WORK/$img"
    mkfs_det 05 -b 4096 -O extent,64bit,flex_bg,metadata_csum -E "$EXT_DET" -L deep-vol "$WORK/$img"
    mount -t ext4 -o loop "$WORK/$img" "$MNT"
    # Sparse file with 1-byte 'X' writes every 64 KiB up to 16 MB —
    # ~245 extents force a multi-level extent tree.
    dd if=/dev/zero of="$MNT/sparse.bin" bs=1 count=0 seek=16M status=none
    local off=0
    while [ "$off" -lt 16000000 ]; do
        printf 'X' | dd of="$MNT/sparse.bin" bs=1 count=1 seek="$off" conv=notrunc status=none
        off=$((off + 65536))
    done
    echo 'control file' > "$MNT/dense.txt"
    finish "$img"
}

build_inline() {
    local img=ext4-inline.img
    truncate -s 8M "$WORK/$img"
    mkfs_det 06 -b 4096 -I 256 -O ext_attr,extent,64bit,filetype,dir_index,metadata_csum,inline_data \
        -E "$EXT_DET" -L inline-vol "$WORK/$img"
    mount -t ext4 -o loop "$WORK/$img" "$MNT"
    echo 'tiny inline' > "$MNT/tiny.txt"
    printf 'A%.0s' $(seq 1 100) > "$MNT/medium.txt"
    ln -s 'target/path/here' "$MNT/symlink"
    finish "$img"
}

build_xattr() {
    local img=ext4-xattr.img
    truncate -s 8M "$WORK/$img"
    mkfs_det 07 -b 4096 -O ext_attr,extent,64bit,filetype,dir_index,metadata_csum,inline_data \
        -E "$EXT_DET" -L xattr-vol "$WORK/$img"
    mount -t ext4 -o loop "$WORK/$img" "$MNT"
    echo 'has xattrs' > "$MNT/tagged.txt"
    setfattr -n user.color -v 'red' "$MNT/tagged.txt"
    setfattr -n user.com.apple.FinderInfo -v '0xDEADBEEF' "$MNT/tagged.txt"
    mkdir "$MNT/tagged_dir"
    setfattr -n user.purpose -v 'documents' "$MNT/tagged_dir"
    echo 'no xattrs here' > "$MNT/plain.txt"
    finish "$img"
}

build_acl() {
    local img=ext4-acl.img
    truncate -s 8M "$WORK/$img"
    mkfs_det 08 -b 4096 -O ext_attr,extent,64bit,filetype,dir_index,metadata_csum -E "$EXT_DET" -L acl-vol "$WORK/$img"
    tune2fs -o acl,user_xattr "$WORK/$img" >/dev/null
    mount -t ext4 -o loop,acl,user_xattr "$WORK/$img" "$MNT"
    echo 'minimal acl' > "$MNT/mode_only.txt"
    setfacl -m u::rwx,g::r-x,o::r-- "$MNT/mode_only.txt"
    echo 'named entries' > "$MNT/named.txt"
    setfacl -m u:1000:rw-,g:2000:r--,m::rwx "$MNT/named.txt"
    mkdir "$MNT/acl_dir"
    setfacl -m u::rwx,g::r-x,o::--x,d:u::rwx,d:g::r-x,d:o::--- "$MNT/acl_dir"
    echo 'no acl' > "$MNT/plain.txt"
    finish "$img"
}

build_largedir() {
    local img=ext4-largedir.img
    truncate -s 192M "$WORK/$img"
    mkfs_det 09 -b 4096 -N 80000 \
        -O has_journal,ext_attr,dir_index,filetype,extent,64bit,flex_bg,sparse_super,large_file,huge_file,uninit_bg,metadata_csum,large_dir \
        -E "$EXT_DET" -L largedir-vol "$WORK/$img"
    mount -t ext4 -o loop "$WORK/$img" "$MNT"
    mkdir -p "$MNT/huge"
    # 70000 empty files, file_00001.txt .. file_70000.txt, created in
    # order. `touch` in batches rather than one process per file, and
    # 2000 names per batch, each batch synced and the volume unmounted and
    # mounted again before the next. One pass over all 70000 names kept
    # every new inode and dentry cached at once: an emulated guest with
    # little memory ran out of it part way. The unmount drops them, so the
    # build needs the same small amount of memory whatever the guest has
    # (an ARM64 guest with 64 MiB and no swap built every fixture this
    # way). The names, their order and the count are what one pass made.
    local first last
    for ((first = 1; first <= 70000; first += 2000)); do
        last=$((first + 1999))
        (cd "$MNT/huge" && seq -f '%05g' "$first" "$last" | sed 's/.*/file_&.txt/' | xargs touch)
        sync
        umount "$MNT"
        mount -t ext4 -o loop "$WORK/$img" "$MNT"
        echo "[guest] largedir: $last of 70000 written, uptime $(cut -d ' ' -f1 /proc/uptime)s"
    done
    echo 'control' > "$MNT/small.txt"
    finish "$img" check_largedir
}

# check_largedir <image> — the unmounted image holds exactly the 70000
# regular files file_00001.txt .. file_70000.txt in /huge, as debugfs
# lists them, and e2fsck finds nothing to fix. Read-only: `debugfs`
# without -w and `e2fsck -n` write nothing.
check_largedir() {
    local image="$1"
    debugfs -R 'ls -p /huge' "$image" | awk -F/ '
        $6 ~ /^file_[0-9][0-9][0-9][0-9][0-9]\.txt$/ {
            number = substr($6, 6, 5) + 0
            if (number < 1 || number > 70000 || $3 !~ /^10/) invalid = 1
            count++
        }
        END {
            if (invalid || count != 70000) { print "[guest] largedir: /huge holds " count " valid names, not 70000" > "/dev/stderr"; exit 1 }
            print "[guest] largedir: /huge verified, " count " regular files"
        }
    '
    e2fsck -fn "$image"
}

build_manyfiles() {
    local img=ext4-manyfiles.img
    truncate -s 16M "$WORK/$img"
    mkfs_det 10 -b 4096 -O has_journal,ext_attr,dir_index,filetype,extent,64bit,flex_bg,sparse_super,metadata_csum \
        -E "$EXT_DET" -L many-vol "$WORK/$img"
    mount -t ext4 -o loop "$WORK/$img" "$MNT"
    local i
    for i in $(seq 1 512); do
        printf 'f%04d\n' "$i" > "$MNT/file_$i.txt"
    done
    finish "$img"
}

build_whole_disk() {
    local img=ext4-whole-disk.img
    # 20 MiB: 1 MiB GPT header area + 16 MiB partition (32768 × 512 B) +
    # the GPT backup.
    truncate -s 20M "$WORK/$img"
    # A fixed disk GUID and partition GUID, for the same reason as -U.
    printf 'label: gpt\nlabel-id: 5E1F1C0D-0000-4000-8000-000000000011\nstart=2048, size=32768, type=L, uuid=5E1F1C0D-0000-4000-8000-000000000012\n' |
        sfdisk --quiet "$WORK/$img"
    # --offset/--sizelimit map the partition directly, with no need for
    # kernel partition scanning on the loop device.
    LOOP="$(losetup -f --show --offset $((2048 * 512)) --sizelimit $((32768 * 512)) "$WORK/$img")"
    mkfs_det 11 -b 4096 \
        -O has_journal,ext_attr,dir_index,filetype,extent,64bit,flex_bg,sparse_super,metadata_csum \
        -E "$EXT_DET" -L wholedisk "$LOOP"
    mount -t ext4 "$LOOP" "$MNT"
    echo 'whole disk test' > "$MNT/test.txt"
    mkdir -p "$MNT/subdir"
    echo 'nested' > "$MNT/subdir/nested.txt"
    ln -s test.txt "$MNT/link.txt"
    sync
    umount "$MNT"
    losetup -d "$LOOP"
    LOOP=""
    cp --sparse=always "$WORK/$img" "$OUTPUT_DIR/$img.partial"
    mv -f "$OUTPUT_DIR/$img.partial" "$OUTPUT_DIR/$img"
    echo "[guest] built $img"
}

for t in "$@"; do
    case "$t" in
        basic) build_basic ;;
        htree) build_htree ;;
        csum_seed) build_csum_seed ;;
        no_csum) build_no_csum ;;
        deep_extents) build_deep_extents ;;
        inline) build_inline ;;
        xattr) build_xattr ;;
        acl) build_acl ;;
        largedir) build_largedir ;;
        manyfiles) build_manyfiles ;;
        whole_disk) build_whole_disk ;;
        *) echo "[guest] unknown image: $t (the list is in test-disks/build-fixtures.sh)" >&2; exit 2 ;;
    esac
done
echo "[guest] done: $(uname -sr)"
