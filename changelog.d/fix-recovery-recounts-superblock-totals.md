### Fixed

- **Recovery recounts the superblock's free block and inode totals.** The
  kernel writes those totals back lazily, so after a crash its log brings the
  group descriptors up to date and leaves the superblock behind; a replay that
  kept them left `e2fsck -fn` reporting "Free blocks count wrong" and "Free
  inodes count wrong". Completing recovery now sets both to the sums over the
  group descriptors, in the same write that clears `needs_recovery` (#507).
  `tests/lazy_write_over_kernel_log.rs` runs the field sequence end to end: a
  kernel crash image, a lazy mount that refuses to write before replay, then a
  directory and its entries in a group still `INODE_UNINIT`, judged by
  `e2fsck` and read back by the kernel (#506).
