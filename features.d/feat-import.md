## Interfaces

| `fs.ext4 import SRC PATH [--replace] [--verify]`: bounded single-file import, complete-file journaled publication and fresh staged readback (`--features cli`) | Supported | Unreleased | | `cli_import_kernel.rs`; requires a journal; stdin spools to disk |
| `fs.ext4 import --recursive`: shared directory-tree import | Unsupported | | #550 | BLOCKED: needs rust-fs-core tree source traits released |
