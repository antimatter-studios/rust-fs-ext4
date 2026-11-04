### Fixed

- **`chore staticlib` copies the archive from where cargo built it.** The task
  read a fixed `target/` while cargo honoured `CARGO_TARGET_DIR` and
  `build.target-dir`, so the copy failed after a good build, or shipped an old
  archive left in `target/` without an error (#624).
