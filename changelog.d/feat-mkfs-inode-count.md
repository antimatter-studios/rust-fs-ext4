### Added
- **The inode count can be chosen at format time.** `mkfs::FormatOptions` and `mkfs::format_filesystem_with_options` take an inode count, as `mke2fs -N` does. The count is spread over the block groups and rounded as `mke2fs` rounds it: up to whole inode-table blocks, then down to whole bitmap bytes, and clamped to 65536 inodes a group less one table block's worth. `mkfs.ext4 -N COUNT` / `--inodes COUNT` sets it; before, `-N` was accepted and ignored with a warning. Without a count, every group still gets 8192 inodes (`DEFAULT_INODES_PER_GROUP`).

### Fixed
- **A layout a short final group cannot hold is refused before anything is written.** Every group is now laid out first. Before, the groups ahead of the short one were written and the format then failed partway through.
