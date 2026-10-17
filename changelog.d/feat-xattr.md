### Added

- **`fs.ext4 xattr PATH [NAME]` lists and reads a path's extended
  attributes.** Without NAME it reports `[{"name": ..., "size": N}, ...]`,
  names with their namespace prefix and sorted by their bytes, or size,
  tab and name with `--text`; with NAME it writes the value's raw bytes to
  stdout or through `FILE.partial` to `-o FILE`, and a missing attribute
  exits 1. It reads through the library's `read_all_resolved`, so values
  in the inode, an external block and an EA inode are all reached, and a
  symlink's own attributes are read, never its target's. Names and values
  are checked against what the kernel's `getfattr` returns for attributes
  Linux set in the user, trusted and security namespaces, with binary and
  empty values, and the inline-data tail is listed as `system.data`, which
  the kernel does not show, on exactly the inodes `lsattr` marks inline
  (#522).
