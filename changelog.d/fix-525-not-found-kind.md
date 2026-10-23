### Changed

- **A missing path is the `not_found` error kind.** `fs.ext4`'s `ls`, `ls -R`, `stat`, `read` and `export` (single file and `--recursive`) report a path that does not exist with `"kind": "not_found"` beside `error` and `code` in the JSON error, still exit 1, so a script tells it by that field rather than by the message. Built on rust-fs-core 0.4.1, for `CliError::not_found`.
