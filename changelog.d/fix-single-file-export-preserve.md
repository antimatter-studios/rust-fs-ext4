### Fixed

- **`fs.ext4 export --preserve` without `--recursive` is refused, not ignored.** A single-file export given `--preserve=links`, `--preserve=times` or both now exits 2 with a usage error naming `--preserve` and `--recursive` before the image is opened, instead of publishing a file whose times were the export's own; `--help` states that `--preserve` requires `--recursive`.
