### Added

- **The journal read as evidence (`fs_ext4::salvage::scan_journal`).** Every
  copy of a filesystem block the journal ring still holds is listed, from
  every transaction found -- checkpointed ones on a clean journal included
  -- with its transaction, whether a commit was found for it, whether the
  logged block was escaped, and its tag checksum where the journal keeps
  one. A directory block destroyed in place reads back from the journal's
  newest copy. Checked against `debugfs logdump` and `e2fsck` on the
  kernel's ext3 and ext4 journals (#649).
