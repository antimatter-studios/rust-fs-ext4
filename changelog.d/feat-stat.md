### Added

- **`fs.ext4 stat PATH` reports one path in full.** Inode number, type,
  permission bits, owner, link count, size, allocated bytes, a symlink's
  target or a device node's numbers, and the access, modification, change
  and creation times as seconds plus nanoseconds, with the inode's flags,
  generation and extended-attribute block under `ext4`. A symlink is
  reported rather than followed, and a field a 128-byte inode has no room
  for is null rather than zero. Every field is checked against the
  kernel's own `stat` on a volume Linux filled (#514).
