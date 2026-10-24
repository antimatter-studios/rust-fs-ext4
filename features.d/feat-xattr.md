## Interfaces

| `fs.ext4 xattr PATH [NAME]`: a path's extended attributes listed as JSON or text names and sizes, or one value written as raw bytes to stdout or `-o FILE`, from the inode, an external block or an EA inode, a symlink's own and never its target's; a name that is not UTF-8 listed lossily with its bytes in `name_hex` and read by the raw bytes of NAME (`--features cli`) | Supported | Unreleased (#522, #541) | | `cli_xattr_kernel.rs`, `tests/cli/test-stat.sh` |
