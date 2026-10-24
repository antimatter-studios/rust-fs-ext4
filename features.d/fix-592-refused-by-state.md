## Interfaces

| `fs.ext4` namespace verbs (`rm`, `rmdir`, `mv`, `ln`, `mkdir`, `truncate`, `write --at`) exit 4 when the volume's state refuses them: not empty, already exists, missing target, not a directory (`--features cli`) | Supported | Unreleased (#592) | | `cli_rm_kernel.rs`, `cli_mv_kernel.rs`, `cli_ln_kernel.rs`, `cli_mkdir_kernel.rs`, `cli_truncate_kernel.rs`, `cli_pwrite_kernel.rs`, `cli_fs_write.rs`, `tests/cli/test-rm.sh` |
