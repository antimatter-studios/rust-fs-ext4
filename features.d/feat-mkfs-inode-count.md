## Making a filesystem

| Inode count chosen at format time (`FormatOptions::inodes_count`, `mkfs.ext4 -N` / `--inodes`), rounded as `mke2fs -N` rounds it | Supported | Unreleased | | `mkfs_inode_count.rs`, `mkfs_inode_count_oracle.rs`, `cli_mkfs.rs` |
