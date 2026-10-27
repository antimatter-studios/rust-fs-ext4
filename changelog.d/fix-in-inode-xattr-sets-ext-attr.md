### Fixed

- **An extended attribute that fits in the inode now sets `ext_attr`.**
  Only the external-block path set the feature, so on a volume made
  without it, as this crate's mkfs makes one, an attribute stored in the
  inode was written where `debugfs ea_list` lists nothing and `e2fsck`
  reads nothing. Setting any attribute now ORs `COMPAT_EXT_ATTR` into the
  superblock, with its checksum, in the same transaction as the attribute.
