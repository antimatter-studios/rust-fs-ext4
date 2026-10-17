## Interfaces

| `fs.ext4 export PATH DEST [--replace]`: bounded single-file export with a SHA-256 report, complete-file publication and raw stdout (`--features cli`) | Supported | Unreleased (#547) | | `cli_export_kernel.rs` |
| `fs.ext4 export --recursive`: shared directory-tree export with inode identity, links and times | Unsupported | | #546 | Awaiting a core release with the shared tree source traits; core 0.3.14 provides only single-file export |
