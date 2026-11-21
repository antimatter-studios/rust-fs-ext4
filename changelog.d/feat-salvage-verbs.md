### Added

- **The salvage verbs (`fs_ext4::salvage::Ext4Salvage`).** Find candidates
  (every inode-table slot that is not all zeros, live or deleted, and every
  superblock copy, streamed to a sink that can stop the scan), walk an
  inode's storage as source byte ranges in file order, rebuild the tree
  from the parent claims directory entries make (orphans and conflicting
  claims kept), report damage across the superblocks, descriptors, inodes,
  maps and directories, read raw source bytes by extent, and materialize a
  plan of source extents, zeros, literal bytes and block runs into a
  caller's writer, recording the provenance of every byte and writing an
  unreadable block as zeros instead of failing. Checked against the kernel
  on `mkfs.ext2`, `mkfs.ext3` and `mkfs.ext4` volumes: every file recovered
  has the kernel's path and SHA-256, also after a directory block is zeroed (#648).
