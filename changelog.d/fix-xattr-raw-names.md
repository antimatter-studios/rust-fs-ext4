### Fixed

- **`fs_ext4_getxattr`, `fs_ext4_setxattr` and `fs_ext4_removexattr` take
  a name that is not UTF-8.** `fs_ext4_listxattr` hands back each name's
  exact bytes, and Linux takes any bytes but NUL in a name, but the calls
  that take a name back refused one that is not UTF-8 with `EINVAL`. They
  now match, replace and remove it by its bytes, checked against names
  the kernel set (#591).

### Added

- `xattr::split_qualified_name_raw`, `plan_set_in_inode_region_raw`,
  `plan_remove_in_inode_region_raw`, `plan_set_in_external_block_raw` and
  `plan_remove_from_external_block_raw`, which take a name as its bytes
  (#591).
