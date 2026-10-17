### Added

- **`fs.ext4 ln TARGET PATH` adds a hard link, and `ln -s` makes a symbolic
  link.** A hard link gives the file TARGET names one more name and one
  more link, a symlink named as TARGET being linked itself; a symbolic
  link's target is the text given, never resolved, inline under 60 bytes
  and in a block from 60. It reports `{"path": ..., "linked": true,
  "inode": N}` and leaves the volume clean. Every refusal (a name already
  there, a missing target or parent, a hard link to a directory, a file at
  65000 links, an empty target or one that does not fit in a block) is
  decided before the image is opened for writing, for the reason the Linux
  kernel gives `linkat` and `symlinkat`. The links are checked by `e2fsck
  -fn` and by the kernel's own mount, and every refusal against the
  kernel's (#518).
