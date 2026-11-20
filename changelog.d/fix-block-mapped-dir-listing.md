### Fixed

- **ext2 and ext3 directories are listed.** `Filesystem::read_dir_ino`, and
  everything built on it, refused every directory mapped by direct and
  indirect block pointers with "legacy (non-extent) dirs not yet
  supported", though path lookup through the same directories worked. It
  now follows either map (#650).
