### Added

- **Damage-tolerant block maps and directory scans (`fs_ext4::salvage`).**
  `map_inode` follows an inode's extent tree or direct and indirect block
  pointers as far as they go, keeping every run it reaches and saying where
  and why it stopped: a node with a wrong magic number, a pointer outside
  the volume, a block reached twice, an unreadable node, or fewer blocks
  than `i_blocks` counts. An extent node whose checksum fails is still
  followed. `read_directory` scans linear, inline and hashed directories,
  picks the record chain up again after a broken record length, and finds
  deleted entries left in a record's slack. Checked against `debugfs`,
  `e2fsck` and the kernel on `mkfs.ext2`, `mkfs.ext3` and `mkfs.ext4`
  volumes the kernel filled, intact and damaged (#647).
