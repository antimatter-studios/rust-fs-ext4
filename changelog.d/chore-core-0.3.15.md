### Changed

- **rust-fs-core 0.3.15.** The crate builds against the core release that
  carries the shared tree source, which recursive export will adapt; the
  release workflow and branch reaping call that release's workflows.
- **The minimum supported Rust is 1.89.** rust-fs-core 0.3.15's `cli`
  feature uses `File::try_lock` and `TryLockError`, stable from Rust 1.89,
  so `rust-version` moves from 1.87 to 1.89.
