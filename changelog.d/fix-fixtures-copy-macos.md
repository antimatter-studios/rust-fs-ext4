### Fixed

- **`chore fixtures` finishes on macOS.** The guest built every image, and the
  copy into `test-disks/` then failed: macOS's `cp` refuses GNU's
  `--sparse=always`. `scripts/copy-sparse.sh` keeps the holes with GNU `cp` and
  uses a plain copy, which APFS clones, with BSD's.
