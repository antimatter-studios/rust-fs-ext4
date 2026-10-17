### Added

- **`fs.ext4 write --at BYTES PATH` writes stdin into a file at a
  position.** Only the bytes written change and nothing is truncated; a
  write past the end grows the file, the gap reading back as zeros and
  allocating nothing, through the library's `apply_pwrite_ino`, in one
  writable open released clean. It reports `{"path": ..., "at": N,
  "bytes": N, "size": N}`, and nothing on stdin writes nothing. The whole
  input is read before the image is opened, and a directory, a missing
  path or parent, a name too long, a file on the way, a symlink (never
  followed), an immutable or append-only file and a position past the
  largest file are refused before it is opened for writing, for the
  reason Linux gives; a block-mapped or encrypted file answers exit 3,
  which this driver cannot write into. The writes are checked against
  twins the kernel's `dd conv=notrunc` wrote, on size, allocated blocks
  and content, by the kernel's own mount and by `e2fsck -fn`, and every
  refusal against the kernel's (#521).
