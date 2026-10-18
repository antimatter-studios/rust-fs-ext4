### Fixed

- **A path whose middle names a symbolic link resolves through the link.**
  `fs.ext4 rm /dirlink/x`, where `dirlink` names a directory holding `x`,
  removed nothing and said "not a directory"; every verb and C entry point
  that takes a path now follows a link met before the final component, as
  the Linux kernel does: an absolute target from the root, a relative one
  from the directory holding the link. The final component is still never
  followed, so `rm` of a link removes the link. More than 40 links in one
  lookup, the kernel's limit, is refused as `ELOOP` ("too many levels of
  symbolic links") through the new `Error::SymlinkLoop`. Checked against
  the kernel's own mount (#531, #564).
