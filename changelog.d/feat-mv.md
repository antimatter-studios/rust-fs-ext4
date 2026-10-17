### Added

- **`fs.ext4 mv SOURCE PATH` gives a name a new name.** In its own
  directory or another, a file, a symlink or a directory with its tree;
  PATH is the new name itself, and a moved directory's `..` and both
  parents' link counts follow it. A name already at PATH is refused unless
  `--replace` is given, which replaces it as rename(2) does: a file by a
  file, an empty directory by a directory. It reports `{"path": ...,
  "moved": true}` and leaves the volume clean. Every refusal is decided
  before the image is opened for writing, for the reason the Linux kernel
  gives `renameat2`. The moves are checked by `e2fsck -fn` and by the
  kernel's own mount, and every refusal against the kernel's (#517).
