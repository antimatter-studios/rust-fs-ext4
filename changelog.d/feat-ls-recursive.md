### Added

- **`fs.ext4 ls --recursive PATH` walks a tree within stated bounds.**
  Each entry is the `ls` entry plus its path from the root of the volume
  and its depth, depth first with each directory's names sorted as bytes,
  so two runs print the same thing. A symlink is listed and never followed,
  and a directory whose inode was already visited is listed as `revisited`
  and not entered again, so a volume whose directories form a cycle still
  ends. `--max-depth` (default 64) and `--limit` (default 100000) bound the
  walk; reaching one, or a directory that cannot be read, exits 1 with the
  entries so far on stdout and the bound or path named on stderr. The
  listing is checked against GNU find's walk of the kernel's own mount
  (#515).
