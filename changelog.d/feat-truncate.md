### Added

- **`fs.ext4 truncate PATH SIZE` sets a regular file's length.** A shrink
  releases the blocks past the new end and a grow reads back as zeros,
  allocating nothing, through the library's `apply_truncate_ino`, in one
  writable open released clean. It reports `{"path": ..., "size": N}`.
  SIZE is a byte count, and anything else is a usage error before the
  image is opened. A directory, a missing path or parent, a name too long,
  a file on the way, a symlink (never followed) and an immutable or
  append-only file are refused before the image is opened for writing,
  for the reason Linux's `truncate` gives; the shrink of a block-mapped
  file and any truncate of an encrypted one answer exit 3, which this
  driver cannot do. The lengths are checked against twins the kernel
  truncated, on size, allocated blocks and content, by the kernel's own
  mount and by `e2fsck -fn`, and every refusal against the kernel's
  (#520).
