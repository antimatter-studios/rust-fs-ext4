## Interfaces

| `fs.ext4 ls`: shared recursive-listing arguments, help, defaults and bound-reached errors (`--features cli`) | Supported | Unreleased (#615) | antimatter-studios/rust-fs-core#406 | `cli_ls_recursive_kernel.rs`, `tests/cli/test-ls-recursive.sh` |
| `fs.ext4 export --recursive` and `import --recursive`: a tree that published some items and left others out exits 5; one that published nothing exits 1 (`--features cli`) | Supported | Unreleased (#615) | antimatter-studios/rust-fs-core#361 | `cli_export_tree_kernel.rs`, `cli_import_tree_kernel.rs` |
