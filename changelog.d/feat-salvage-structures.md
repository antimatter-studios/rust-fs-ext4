### Added

- **Damage-tolerant reads of the superblock, its backups, the group
  descriptor tables and inodes (`fs_ext4::salvage`).** Each structure is
  read where the format puts it and returned with how far it can be
  believed -- a failed checksum or a pointer outside the volume is reported,
  not refused -- and every backup copy is listed whether or not it parses.
  When the primary superblock is unusable the geometry comes from the first
  sound backup, found through the primary's own layout or by probing the
  default backup groups. `structure_damage` lists faulty and disagreeing
  copies and the state the superblock records. The source is a
  `fs_core::BlockRead`, so these reads cannot write. Checked against
  `dumpe2fs`, `debugfs` and `e2fsck` on volumes `mkfs.ext2`, `mkfs.ext3` and
  `mkfs.ext4` made, with a zeroed primary superblock and a broken
  descriptor checksum (#646).
