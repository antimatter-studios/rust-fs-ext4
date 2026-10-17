### Added

- **`fs.ext4 rm PATH` and `fs.ext4 rmdir PATH` remove a name.** `rm`
  takes away a name that is not a directory (a symlink, never what it
  points at), and the file goes with its last name; `rmdir` takes away an
  empty directory. Both report `{"path": ..., "removed": true}` and leave
  the volume clean. A missing path, a directory given to `rm`, anything but
  a directory given to `rmdir`, a directory with entries, `.`, `..` and the
  root are refused, for the reason the Linux kernel gives, before the image
  is opened for writing. The removals are checked by `e2fsck -fn` and by the
  kernel's own mount, and every refusal against the kernel's (#516).
