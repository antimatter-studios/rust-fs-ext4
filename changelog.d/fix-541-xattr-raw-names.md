### Fixed

- **An extended attribute name that is not UTF-8 no longer makes every
  attribute on its inode unreadable.** Linux takes any bytes but NUL in a
  name, and both entry parsers refused such a name as corrupt, so
  `read_all`, `read_all_resolved`, `list_names`, `get` and `get_resolved`
  failed for the whole inode, and so did reading an inline file holding
  one, whose tail is found as `system.data`. `fs.ext4 xattr` now lists
  such a name lossily with its exact bytes in `name_hex`, as the shared
  CLI contract defines, and finds NAME by its raw bytes, checked against
  names the kernel set (#541).

### Added

- `XattrEntry::raw_name`, the full name's exact bytes, beside `name`, now
  their lossy form; `XattrEntry::with_raw_name`; `xattr::get_resolved_raw`
  and `xattr::list_raw_names`, which take and give names as bytes.
  `fs_ext4_listxattr` hands back each name's exact bytes (#541).
