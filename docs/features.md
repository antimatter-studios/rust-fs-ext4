# Features

What this driver does today, what it refuses, and what is coming. **Every
pull request that adds, fixes, refuses or removes behaviour updates its row
here, in the same pull request** (AGENTS.md). The reasoning behind each change
is in [CHANGELOG.md](../CHANGELOG.md); the feature-bit decisions are in
[format-conformance-gaps.md](format-conformance-gaps.md) and the write plan in
[ext4-full-write-support.md](ext4-full-write-support.md).

**Since** is the release a row's current state shipped in, with the issue or
pull request that landed it where the history names one. Work merged after
the last release is **Unreleased (#N)** until the next one. **Tracking** names
the open issue, or the write-plan item, for anything not finished.

States:

- **Supported**: works, and is checked against e2fsprogs, the Linux kernel or
  lwext4 in the harness VM.
- **Experimental**: works in every test, but is new.
- **Partial**: works for part of the case, and the row says which part.
- **Refused**: recognised and refused by name, rather than misread.
- **Not supported**: neither read nor refused by name.
- **Upcoming**: an open issue with a plan.

## Reading

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| ext2, ext3 and ext4 volumes; 1, 2 and 4 KiB blocks | Supported | 0.1.0 | | `end_to_end.rs`, `ext2_basic.rs`, `oracle_debugfs.rs` |
| Inodes with extra fields; `stat`, `readdir`, `readlink`, `read` | Supported | 0.1.0 | | `inode_basic.rs`, `readlink_oracle.rs`, `kernel_readback_capi.rs` |
| Extent trees, any depth; uninitialised extents read as zeros | Supported | 0.1.0 | | `capi_deep_extents.rs`, `leading_hole_deep_tree.rs` |
| Indirect block maps: direct, single, double, triple | Supported | 0.1.0 | | `ext2_basic.rs` |
| HTree directories: legacy, `half_md4`, `tea` hashes | Supported | 0.1.0 | | `htree_lookup.rs`, `htree_index_descent_oracle.rs`, `htree_hash_differential.rs` |
| Large directories (`large_dir`) | Supported | 0.1.0 | | `largedir.rs`, `largedir_basic.rs` |
| Inline-data files, including the `system.data` spill | Supported | 0.1.0; spill checked 0.5.0 | | `capi_inline_data.rs`, `inline_data_basic.rs` |
| Inline-data directories | Partial: entries that spilled into `system.data` are not found | 0.6.0 | format-conformance-gaps.md G3 | `inline_dir_kernel_reads.rs` |
| Extended attributes: in-inode, external block, shared external blocks, EA inodes | Supported | 0.1.0; shared 0.6.0 (#246) | | `xattr_basic.rs`, `xattr_external_block.rs`, `shared_xattr_block.rs` |
| POSIX ACLs (`system.posix_acl_*`) | Supported | 0.1.0 | | `acl_basic.rs`, `kernel_readback.rs` |
| `metadata_csum`, `csum_seed`, `gdt_csum` verified on every checksummed block | Supported | 0.1.0 | | `checksum_corruption.rs`, `repro_csum_seed_op_coverage.rs` |
| `meta_bg` group descriptors | Supported | 0.6.0 (#235) | | `meta_bg_oracle.rs` |
| `sparse_super2` backup superblocks | Supported | 0.6.0 | | `sparse_super2_backup_bgs_oracle.rs` |
| 32-byte descriptors on non-64bit volumes | Supported | 0.6.0 | | `non_64bit_desc_size.rs` |
| Timestamps past 2038, epoch-extension bits | Supported | 0.5.0; automatic times 0.6.0 (#324) | | `timestamps_past_2038_oracle.rs` |
| JBD2 replay on a writable mount: descriptor, commit and revoke blocks, v2 and v3 checksums | Supported | 0.1.0; checksums 0.6.0 (#229) | | `journal_replay.rs`, `jbd2_checksums_oracle.rs`, `journal_mount_recovery.rs` |
| Replay into memory on a read-only mount, the device untouched | Supported | 0.6.0 (#234) | | `ro_mount_replays_in_memory.rs` |
| Lazy replay (`mount_rw_with_callbacks_lazy`) | Supported | 0.1.4 | | `capi_lazy_replay.rs`, `lazy_replay_reloads_state.rs` |
| Orphan list read and replayed | Supported | 0.1.4 | | `orphan_list_basic.rs`, `orphan_recovery.rs` |
| Names that are not UTF-8 | Supported | 0.6.0 (#420) | | `non_utf8_names_oracle.rs`, `capi_non_utf8_path.rs` |
| `bigalloc` volumes | Supported, read-only | 0.6.0 (#237) | | `bigalloc_read_oracle.rs` |
| `encrypt` volumes: plain files read, encrypted inodes refused by name | Partial | 0.6.0 (#236) | | `encrypt_per_inode.rs` |
| Casefolded directories | Partial: names found by a linear scan; case-insensitive lookup does not work | 0.3.0 | write plan 9.3 | |
| `mmp` volumes | Supported, read-only | 0.5.0 | format-conformance-gaps.md G4 | `feature_matrix.rs` |
| An unknown incompatible feature bit | Refused | 0.1.0 | | `src/features.rs` unit tests |
| A hostile or truncated superblock, inode or descriptor | Refused | 0.5.1 | | `hostile_superblock.rs`, `bgd_pointer_bounds.rs` |
| Fuzzed decoders | Supported | 0.6.0 (#281) | | `fuzz_decoders.rs`, `fuzz_smoke.rs` |
| fs-verity, fscrypt contents | Not supported | | write plan 9.6, 9.7 | |
| Third-implementation cross-check against lwext4 | Supported | 0.6.0 | | `lwext4_cross_validate.rs` |

## Checking

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| `fsck.ext4` / `Filesystem::audit`: link counts, dangling entries, directory-block checksums, uninitialised groups, wrong `..`; fsck(8) exit status | Supported, a subset of e2fsck | 0.1.4; tool 0.6.0 (#446) | | `fsck_audit.rs`, `cli_fsck.rs`, `cli_fsck_oracle.rs`, `fsck_dir_block_checksum_oracle.rs` |
| A directory the audit cannot read reported as a finding | Supported | 0.7.0 (#445) | | `fsck_unreadable_dir.rs`, `fsck_unreadable_dir_oracle.rs` |
| Repair (`-y`): duplicate directory entries, link-count drift | Partial: those two findings only | 0.1.4 | write plan 6.4 | `fsck_repair.rs` |
| Bitmap-against-tree verifier (`verify::verify`) | Supported | 0.1.4 | | `verify_basic.rs` |
| Auditing a `bigalloc` volume | Refused by name | 0.6.0 (#237) | | `bigalloc_read_oracle.rs` |

## Writing

Every multi-block operation commits through the JBD2 writer with four fences,
and a crash at any write leaves the volume either before or after the
operation. Writes are checked by `e2fsck -fn`, `debugfs` and the Linux kernel
reading the result back in the guest.

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| `create`, `unlink`, `mkdir`, `rmdir` | Supported | 0.1.0 | | `capi_create.rs`, `capi_unlink.rs`, `capi_mkdir_rmdir.rs`, `journal_writer_crash_dir_ops.rs` |
| `link`, `symlink` (fast and slow) | Supported | 0.1.0 | | `capi_link.rs`, `capi_symlink.rs`, `capi_long_symlink.rs` |
| `rename`: POSIX no-clobber and overwrite, cross-directory `..` | Supported | 0.1.0 | | `capi_rename_semantics.rs`, `capi_rename_overwrite.rs`, `journal_writer_crash_rename_write.rs` |
| `chmod`, `chown`, `utimens`, `set_flags` | Supported | 0.1.0; `set_flags` 0.3.0, kernel-checked 0.6.0 | | `capi_chmod_chown.rs`, `capi_utimens.rs`, `set_flags_oracle.rs` |
| `mknod` | Supported | 0.3.0 | | `capi_ino_api.rs`, `rename_file_type_oracle.rs` |
| Positional write (`pwrite`), allocating | Supported | 0.2.0 | | `capi_pwrite.rs`, `pwrite_chunking_oracle.rs`, `pwrite_enospc_oracle.rs` |
| Whole-file replace | Supported | 0.1.0 | | `capi_write_file.rs`, `capi_write_file_allocates_blocks.rs` |
| Truncate, shrink and grow (sparse) | Supported | 0.1.0 | | `capi_truncate.rs`, `capi_truncate_grow.rs`, `sparse_grow.rs`, `journal_writer_truncate_shrink.rs` |
| Truncate of a file mapped by indirect blocks (ext2/ext3) | Refused | 0.6.0 | | |
| Extent trees: promotion and inserts at any depth | Supported | 0.1.4 | | `extent_deep_insert.rs`, `extent_multi_level.rs`, `truncate_multilevel.rs` |
| `fallocate`: `KEEP_SIZE`, `PUNCH_HOLE`, `ZERO_RANGE`, unaligned ranges | Supported | 0.1.4 | | `fallocate_keep_size.rs`, `fallocate_punch_zero.rs`, `punch_unaligned_oracle.rs`, `fallocate_crash_safety.rs` |
| Extended attributes: in-inode and external block, set and remove | Supported | 0.1.0 | | `capi_setxattr.rs`, `capi_removexattr.rs`, `xattr_one_copy_oracle.rs` |
| External xattr blocks shared between inodes (EA refcount) | Not supported: a write never shares one | | write plan 3.5 | |
| Inline-data files: write, replace and truncate, converting to extents | Supported | 0.6.0 (#415) | | `inline_file_writes_oracle.rs`, `kernel_inline_writes.rs` |
| Inline-data directories: entries added and removed, converting to a block | Supported | 0.6.0 (#414) | | `inline_dir_writes_oracle.rs` |
| HTree directories: inserts and leaf splits in one transaction | Supported | 0.6.0 (#302, #347) | | `htree_dir_writes_oracle.rs`, `htree_split_write_cut.rs` |
| HTree interior index node split | Partial: a full interior node is refused | | | |
| ext2 and ext3 writes (indirect block maps) | Supported | 0.4.0 (#22) | | `mkfs_ext3_oracle.rs`, `ext2_basic.rs` |
| An ext2/ext3 directory that needs the double-indirect block | Refused | 0.6.0 | | `src/fs.rs` unit tests |
| Multiple block groups, uninitialised groups | Supported | 0.4.0; uninit groups 0.4.1, 0.6.0 | | `allocations_into_an_uninit_group.rs`, `uninit_group_flags_cleared.rs` |
| A volume with a read-only-compatible bit the writer does not maintain (quota, project, orphan file, bigalloc) | Refused for writing | 0.6.0 (#77) | write plan 9.4, 9.5 | `ro_compat_write_guard.rs` |
| Writing an `mmp`, `casefold` or `encrypt` volume | Refused | 0.6.0 (#76) | | `feature_matrix.rs` (mmp), `encrypt_per_inode.rs` |
| JBD2 journal modes other than ordered | Not supported | | write plan 5.3 | |
| Orphan-list insert on an unlink of an open file | Not supported | | write plan 6.3 | |
| Volume label, set after the volume is made | Supported | 0.7.0 (#447) | | `volume_label.rs`, `volume_label_oracle.rs`, `capi_set_volume_label.rs` |
| Resize | Not supported (`not implemented`, exit 3) | 0.6.0 | | `cli_fs_images.rs` |

## Making a filesystem

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| `mkfs.ext4` / `fs_ext4_mkfs`: ext2, ext3 and ext4, multiple groups, `/lost+found` | Supported | 0.4.0 (#22); `/lost+found` 0.7.0 (#443) | | `mkfs_e2fsck_oracle.rs`, `mkfs_ext3_oracle.rs`, `mkfs_lost_found_oracle.rs`, `cli_mkfs_oracle.rs` |
| 16 KiB blocks and larger | Supported | 0.6.0 | | |

## Interfaces

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| C ABI: every mount, read and write entry point above, by path or by inode | Supported | 0.1.0; inode-addressed 0.6.0 (#419) | | `capi_basic.rs`, `inode_api.rs`, `capi_header_names_real_functions.rs`, `kernel_readback_capi.rs` |
| `fs_core` device mount | Supported | 0.6.0 (#365) | | `capi_fs_core_mount.rs` |
| `fs_ext4_flush`, `fs_ext4_fresh_read` | Supported | 0.6.0 (#405) | | `capi_flush.rs` |
| Paths as bytes, not UTF-8 | Supported | 0.6.0 | | `capi_non_utf8_path.rs`, `capi_paths.rs` |
| Concurrent callers | Supported | 0.1.0 | | `capi_concurrency.rs` |
| `wasm32-unknown-unknown` build, run under Node | Supported | 0.6.0 (#312) | | `wasm.rs` |
| `fs.ext4` `ls`, `read`, `write`, `mkdir`, `get`/`info`, `set label` (`--features cli`) | Supported | 0.6.0 (#444, #448); label 0.7.0 (#447) | | `cli_fs_images.rs`, `cli_fs_write.rs`, `cli_fs_write_oracle.rs`, `cli_fs_kernel.rs` |
| `rust-fs-ext4 doctor`, man pages, shell completions | Supported | 0.6.0 | | `cli_dispatch.rs`, `cli_docs.rs`, `one_binary_on_path.rs` |
