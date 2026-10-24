## Interfaces

| `fs.ext4` reading verbs (`ls`, `ls -R`, `stat`, `read`, `export`) report a missing path as the `not_found` error kind, exit 1 (`--features cli`) | Supported | Unreleased (#595) | | `cli_fs_images.rs`, `cli_export_kernel.rs`, `tests/cli/test-fs.sh`, `tests/cli/test-stat.sh`, `tests/cli/test-ls-recursive.sh` |
