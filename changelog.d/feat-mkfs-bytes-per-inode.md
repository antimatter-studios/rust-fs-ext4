### Changed
- **A formatted volume's inode count follows its size, as `mke2fs` sizes it.** Without an inode count, `format_filesystem` and `mkfs.ext4` give one inode per 8 KiB below 3 MiB, per 4 KiB below 512 MiB, and per 16 KiB from there (32 KiB from 4 TiB, 64 KiB from 16 TiB). Before, every group got 8192 inodes whatever the block size. That spent about a quarter of a 1 KiB-block volume on inode tables, and left large 64 KiB-block volumes with a small fraction of the inodes `mke2fs` gives them. At 4 KiB blocks on volumes of 512 MiB or more, the count is unchanged.

### Added
- **The bytes-per-inode ratio can be chosen at format time.** `FormatOptions::bytes_per_inode` takes it, as `mke2fs -i` does, from 1 KiB to 64 MiB, and `mkfs.ext4 -i BYTES` / `--bytes-per-inode BYTES` sets it; before, `-i` was accepted and ignored with a warning. An inode count (`-N`) takes precedence over it. `mkfs::default_bytes_per_inode` gives the ratio used when neither is chosen.
