### Added

- **`fs.ext4 mkdir --parents PATH` (`-p`) makes every missing directory on
  the way.** Each is made mode 0755, all in one writable open released
  clean, and the report `{"path": ..., "created": [...]}` lists only the
  directories made. A chain already there all the way is success, with
  nothing created and nothing written. A file, a symlink or a name too long
  anywhere on the way, and an empty path, are refused before the image is
  opened for writing, for the reason Linux's `mkdir -p` gives, so a refused
  command makes no part of the chain. A symlink is never followed. The
  chains are checked by `e2fsck -fn` and by the kernel's own mount, and
  every refusal against the kernel's (#519).
