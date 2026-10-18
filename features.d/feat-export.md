## Interfaces

| `fs.ext4 export PATH DEST [--replace]`: bounded single-file export with a SHA-256 report, complete-file publication and raw stdout (`--features cli`) | Supported | Unreleased (#547) | | `cli_export_kernel.rs` |
| `fs.ext4 export --recursive`: shared directory-tree export with inode identity, links and times | Unsupported | | #546 | core 0.3.15 provides the shared tree source; ext4 does not adapt it yet |
