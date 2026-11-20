#!/usr/bin/env bash
set -euo pipefail

matches=$(grep -R -n -E --include='*.rs' \
    '(^|[^[:alnum:]_])(std::)?fs::read[[:space:]]*\(|\.read_to_end[[:space:]]*\(' tests || true)
if [ -n "$matches" ]; then
    printf 'whole-file reads are forbidden in tests; use bounded helpers:\n%s\n' "$matches" >&2
    exit 1
fi

calls=$(grep -R -n -E --include='*.rs' 'read_small_file[[:space:]]*\(' tests \
    | grep -v -E '^tests/support/src/lib.rs:[0-9]+:pub fn read_small_file\(' || true)
while IFS='|' read -r file expected reason; do
    actual=$(printf '%s\n' "$calls" | grep -c -F "$file:" || true)
    if [ "$actual" -ne "$expected" ]; then
        printf '%s: expected %s bounded small-file reads, found %s (%s)\n' \
            "$file" "$expected" "$actual" "$reason" >&2
        exit 1
    fi
    calls=$(printf '%s\n' "$calls" | grep -v -F "$file:" || true)
done <<'ALLOWLIST'
tests/cli_export_kernel.rs|2|literal destination sentinels of 7 and 20 bytes
tests/cli_export_tree_kernel.rs|1|literal destination sentinel of 7 bytes
tests/cli_fs_images.rs|1|fixture file containing 16 bytes of text
tests/cli_fs_write_oracle.rs|1|debugfs dumps of generated payloads smaller than 1 MiB
tests/cli_transfer_xattrs_kernel.rs|1|literal exported file containing 5 bytes
tests/cli_xattr_kernel.rs|1|single extended attribute bounded by the inode format
tests/fuzz_decoders.rs|1|checked-in decoder corpus seeds smaller than 1 MiB
tests/journal_escape_oracle.rs|1|debugfs dumps of generated journal payloads smaller than 1 MiB
tests/journal_multi_descriptor_oracle.rs|1|debugfs dump of a 512 KiB payload
tests/lwext4_cross_validate.rs|1|checked-in text patch smaller than 1 MiB
tests/punch_unaligned_oracle.rs|1|debugfs dumps of generated payloads smaller than 1 MiB
tests/support/src/oracle.rs|1|bounded oracle diagnostic streams, never disk images
tests/test_image_io.rs|3|small-file boundary and oversized-file refusal tests
ALLOWLIST

if [ -n "$calls" ]; then
    printf 'small-file reads need a reviewed size justification in the allowlist:\n%s\n' "$calls" >&2
    exit 1
fi
