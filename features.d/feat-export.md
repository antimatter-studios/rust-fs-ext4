## Interfaces

| `fs.ext4 export PATH DEST [--replace]`: bounded single-file export with a SHA-256 report, complete-file publication and raw stdout (`--features cli`) | Supported | Unreleased (#547) | | `cli_export_kernel.rs` |
| `fs.ext4 export --recursive PATH DEST`: shared directory-tree export with a per-item report, existing-file refusal and `--max-depth` | Supported | Unreleased (#573) | | `cli_export_tree_kernel.rs` |
| `fs.ext4 export --recursive --preserve=links,times`: hard links, symlink targets and inode times | Supported | Unreleased (#574) | | `cli_export_tree_kernel.rs` |
