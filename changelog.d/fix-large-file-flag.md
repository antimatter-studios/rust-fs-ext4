### Fixed
- **A formatted volume carries `large_file`.** `format_filesystem` and `mkfs.ext4` now format every flavor with `RO_COMPAT_LARGE_FILE`, as `mke2fs` does by default.
