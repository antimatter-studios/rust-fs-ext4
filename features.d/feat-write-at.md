## Interfaces

| `fs.ext4 write --at`: stdin written into a regular file at a position through the library's positional write, nothing truncated and a gap past the end reading back zeros, refusals decided read-only for the reasons the kernel's `dd conv=notrunc` gives, a block-mapped or encrypted file answering exit 3 (`--features cli`) | Supported | Unreleased (#521) | | `cli_pwrite_kernel.rs`, `tests/cli/test-write.sh` |
