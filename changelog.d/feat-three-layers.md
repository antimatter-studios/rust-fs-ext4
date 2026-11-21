### Added

- **The three API layers of rust-fs-core (`fs_ext4::layers`).**
  `Ext4Volume` implements `VolumeAPI` and `OpenVolume` (open, or open for
  salvage from a backup superblock; geometry, every superblock copy,
  support and a quick health verdict), `PrimitiveAPI` (the on-disk
  structures, and the salvage verbs through `Ext4Adapter`, a
  `SalvageAdapter` whose clusters are the volume's blocks) and `FileAPI`
  (browse, metadata, read and links, by inode number). rust-fs-core's
  conformance checks pass on every committed fixture but the 70,000-entry
  directory and on ext2, ext3 and ext4 volumes the kernel filled, and its
  salvage conformance set passes on healthy and damaged volumes (#651).

### Changed

- **rust-fs-core 0.5, with its `layers` feature, and serde.** The crate
  builds against the rust-fs-core release that defines the three layers;
  serde (MIT/Apache-2.0) derives the candidate detail they carry.
- **Inline files past 60 bytes are mapped whole.** The salvage map of an
  inline-data inode now includes the in-inode `system.data` attribute that
  holds bytes past `i_block`, so a walk reads the whole file.
- **Candidates carry a name.** Each inode candidate is named by the first
  live entry naming it in a live directory; the rebuilt tree gives every
  name, one claim per hard link.
- **Uninitialized extents are kept in a walk.** The crate's own walk lists
  an uninitialized extent with its blocks, read as zeros, instead of as a
  hole.
