### Fixed
- Growing a regular file beyond 2 GiB - 1 stages the `large_file` feature flag in the same transaction as its inode, including when another caller creates a file on the same mount.
