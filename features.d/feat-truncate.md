## Interfaces

| `fs.ext4 truncate`: a regular file's length set through the library's truncate, a shrink releasing the blocks past the new end and a grow reading back zeros, refusals decided read-only for the reasons the kernel's `truncate` gives, the shrink of a block-mapped file and an encrypted file answering exit 3 (`--features cli`) | Supported | Unreleased (#520) | | `cli_truncate_kernel.rs`, `tests/cli/test-write.sh` |
