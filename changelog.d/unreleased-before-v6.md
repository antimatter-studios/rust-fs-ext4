### Added

- **`docs/features.md`, a features page kept current by every pull request.**
  Each feature's state, the release it shipped in, its tracking issue and the
  test that checks it. The README's status, "What works", "What doesn't work"
  and roadmap sections, which still called extent-tree mutation at depth two
  unsupported, are a short summary pointing to it.

### Changed

- **The family's scripts run in place from rust-fs-core, and this repository
  keeps no copy.** `scripts/core.sh` and `scripts/tier.sh` are gone; CI and
  chores run `../rust-fs-core/scripts/NAME.sh` at the pinned version
  (rust-fs-core 0.3.3, #212).
- **A release's notes are its CHANGELOG section**, and a tag the CHANGELOG
  does not describe stops before anything is published (rust-fs-core#209).

### Fixed

- **A transient HTTP 5xx from the chore release download no longer fails a CI
  job.** `scripts/ci-install-chore.sh` retries both downloads up to five times on any error; the checksum check still
  guards what was fetched.

### Removed

- **`cargo install` puts only `rust-fs-ext4` on PATH.** The standalone
  `mkfs_ext4` target, which every `cargo install` installed beside the
  multi-call binary, is gone; `mkfs.ext4` is the multi-call binary run under
  that name, with the same flags. `tests/one_binary_on_path.rs` refuses any
  second `[[bin]]` (#498).
