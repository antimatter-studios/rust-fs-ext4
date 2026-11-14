### Fixed
- **A formatted volume carries `large_file`.** `format_filesystem` and `mkfs.ext4` now format every flavor with `RO_COMPAT_LARGE_FILE`, as `mke2fs` does by default.
- **Without a journal, `large_file` reaches the disk before the size that needs it.** The flag is written to the superblock before the enlarged inode, so power lost partway through the write never leaves a file past 2 GiB - 1 on a volume without it.
