### Fixed
- High-level writes on one mount are serialized from path resolution and staging through commit, preventing duplicate inode allocation and lost metadata updates between concurrent Rust or C ABI callers while keeping readers independent of the write lock.
