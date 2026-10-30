## Making a filesystem

| Inode count from a bytes-per-inode ratio, by default by volume size as `mke2fs` chooses it (`FormatOptions::bytes_per_inode`, `mkfs.ext4 -i` / `--bytes-per-inode`) | Supported | Unreleased (#608) | | `mkfs_default_inode_count.rs`, `mkfs_bytes_per_inode.rs`, `mkfs_default_inode_count_oracle.rs`, `cli_mkfs.rs` |
