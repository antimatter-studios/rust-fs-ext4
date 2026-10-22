### Fixed

- **Truncate grow is refused above the file-size limit of its ext4 block
  mapping.** Extent-mapped and indirect-mapped files now share the kernel's
  size boundary, including `EFBIG` at the first byte beyond it. Kernel-oracle
  coverage checks the exact boundary and the first refused byte with 1 KiB
  and 4 KiB blocks, and with `huge_file` enabled and disabled (#538).
