### Added
- `fs.ext4 TARGET export --recursive PATH DEST` copies a directory tree through rust-fs-core 0.3.15's shared tree transfer, over ext4 lookup, listing, stat and positioned reads. Every item is reported, existing files are refused unless `--replace` is given, `--max-depth` bounds the walk, and symlinks and special files are skipped rather than followed.
- `fs.ext4 export --verify` reads a staged file back before publishing it.
