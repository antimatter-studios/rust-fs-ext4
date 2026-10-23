### Fixed

- **A trailing slash follows a symbolic link to a directory.** Path lookup
  now resolves `/dirlink/` to the directory the link names, so `fs.ext4 ls`
  and `stat` agree with Linux. A trailing slash on a file, including one
  inside an expanded link target, is refused as not a directory. Without
  the slash, the final component still names the link itself, so removing
  it leaves its target intact. Checked against the kernel's own mount (#559).
