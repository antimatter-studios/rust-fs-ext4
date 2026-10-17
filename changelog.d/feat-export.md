### Added
- `fs.ext4 TARGET export PATH DEST [--replace]` copies a regular file through rust-fs-core 0.3.14's shared bounded transfer, reports its byte count and SHA-256, and publishes a local destination only after a complete copy. `DEST -` writes raw bytes to stdout. Directories and symlinks are refused.

### Changed
- Every rust-fs-core build, fuzz and workflow pin now names 0.3.14.
