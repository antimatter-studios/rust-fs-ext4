### Fixed

- **Symlink targets leave room for their terminating byte.** The library's
  path and inode creation APIs now reject targets of 1024 bytes on 1 KiB
  volumes and 4096 bytes on 4 KiB volumes with `NameTooLong` before writing
  anything, matching the kernel and the CLI's existing precheck. Targets
  one byte shorter remain accepted and are checked through the kernel's
  own `symlinkat` and `readlinkat` calls (#536).
