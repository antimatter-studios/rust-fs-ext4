### Fixed
- `fs.ext4 TARGET import --recursive --preserve=times` refuses, in that item's report entry, a time the destination inode cannot hold (outside 1901..2446, or past 2038 on a volume of 128-byte inodes) instead of reporting it as a failure; the time is never stored truncated.
