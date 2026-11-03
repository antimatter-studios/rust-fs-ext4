### Changed
- `fs.ext4 ls --recursive` takes its arguments, help, default bounds and bound-reached errors from rust-fs-core's shared listing interface. The defaults, the walk, the error text and the exit statuses are unchanged; the help for `--recursive` now reads "Walk depth first, names sorted as bytes; never follow a symlink".
- `fs.ext4 export --recursive` and `import --recursive` exit 5 instead of 1 when they published some items of a tree and left others out, through rust-fs-core 0.4.3. A script that checks for exactly 1 needs updating.
