### Added

- **`export` and `import --recursive --preserve=xattrs` carry extended
  attributes.** `user.`, `trusted.` and `security.` attributes travel name
  for name and byte for byte, names that are not UTF-8 included. An item
  holding an attribute the other side cannot hold, such as a `trusted.`
  attribute exported by an unprivileged user or a `system.` attribute like
  an ACL, is refused by name in the report rather than dropped.
  `--preserve=sparse` and `--preserve=xattrs` cannot be combined in one
  import.
- **`Filesystem::apply_setxattr_bytes` is public.** It sets an attribute
  whose path and name are given as bytes, never decoded.
