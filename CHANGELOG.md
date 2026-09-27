# Changelog

## [Unreleased]

### Breaking

The next release is **0.6.0**, not 0.5.2: each change below breaks code
written against 0.5.1 (#120). `chore check:semver` now refuses a pull request
whose public-API break the version does not declare (see Added).

Source-breaking, each caught by the compiler downstream:

- **`XattrEntry` gained `value_inum` and `value_size`** (#101, #121), and is
  now `#[non_exhaustive]`, so a struct literal or an exhaustive pattern
  outside this crate no longer compiles. The fields stay public to read;
  build one with `XattrEntry::new(name, value)` for an inline value or
  `XattrEntry::in_ea_inode(name, inum, size)` for one held in an EA inode.
- **`Error` gained `Unsupported(&'static str)`** (#101), and is now
  `#[non_exhaustive]`: a `match` on it outside this crate needs a `_` arm,
  and later variants will not break it again.
- `ea_inode::read_value_inode` takes the size the entry declared as a third
  argument, and refuses a body that does not match it (#121).
- `journal::ReplayPlan` gained `next_sequence: Option<u32>` (#146).
- `extent_mut::ExtentMutation` gained `WriteTreeBlock`, which moved the
  implicit discriminants of `AllocLeafBlock`, `FreeLeafBlock` and
  `FreePhysicalRun` up by one (#169).

Not caught by the compiler — the same source builds and behaves differently:

- **`fs_ext4_readlink` returns the target's length, and refuses a buffer
  it would have to truncate.** It returned 0 on success, so a caller
  slicing its buffer by the return value, as `readlink(2)` callers do, got
  an empty target for every symlink (#290). On success it now writes the
  target and a NUL and returns the target's length in bytes, not counting
  the NUL. A buffer smaller than length + 1 fails with -1 and errno
  `ERANGE`, and nothing is written to it. The message names the size
  needed. Unlike `readlink(2)`, the target is never silently truncated.
  A `bufsize` of 0 with a non-NULL buffer is also `ERANGE`. Only a NULL
  fs, path or buffer is `EINVAL`.
  Every other failure is -1 with the errno set, including a target
  declared longer than any path, which used to set only the message.
  **Callers that test `== 0` for success must test `>= 0`**, and a caller
  that relied on truncation must size its buffer or handle `ERANGE`.
  The target is also available from Rust as `Filesystem::read_link`.
  `tests/readlink_oracle.rs` checks it against what `debugfs` reports,
  for fast and slow links on either side of the 60-byte `i_block` boundary.
- **`XattrEntry` equality covers the two new fields.** It derives
  `PartialEq`, so an entry whose value lives in an EA inode no longer equals
  an inline entry with the same name and (empty) value.
- `plan_set_in_inode_region` and `plan_remove_in_inode_region` refuse an
  attribute held in an EA inode with `Error::Unsupported`, where they used
  to proceed (#101).
- A write to a volume carrying a `RO_COMPAT` feature this driver does not
  maintain is refused, where it used to succeed: the read-only feature rules
  now gate writes, `QUOTA` among them (#77), and `GDT_CSUM` without
  `METADATA_CSUM` is no longer counted as maintained (#92).
- `features::READ_BREAKING_RO_COMPAT` is now `0`: `BIGALLOC` volumes mount
  and read rather than being refused (#237).

### Added

- **The parsers are fuzzed, on two tiers.** ext4 is the widest parser
  surface in the family — a superblock, group descriptors, an inode
  table, extent trees, htree indexes and a jbd2 journal, each read from
  an offset the one before it supplied — and none of it had a fuzz
  target. `fuzz/` holds `image`, `superblock`, `inode`, `dir_block` and
  `journal`, nightly on a bounded budget; `tests/fuzz_decoders.rs` is the
  gate, 23,616 deterministic cases in about four seconds on the stable
  toolchain.

  The corpus is four filesystems `mke2fs` wrote, populated through `-d`
  rather than by mounting: ext2, which has neither extents nor a journal;
  ext4 at 1 KiB blocks, which moves every offset; ext4 at 4 KiB; and ext4
  with `64bit` and `metadata_csum`, which widens the group descriptors.
  Each root holds more than 600 entries, which is what pushes ext4 into
  an htree index — `every_committed_filesystem_mounts_and_reads_its_root`
  asserts that, so the indexed-directory path cannot quietly stop being
  seeded.

  The walk calls `replay_journal_if_dirty`, deliberately. A journal is a
  structure the format expects to be partially written, so it is parsed
  with a corruption tolerance the other structures do not have, and it
  runs at mount before anything has been established (#71).


- **Cross-validation against lwext4, a third implementation, in the
  harness guest (#99).** `scripts/vm-setup.sh` now builds
  [lwext4](https://github.com/gkostka/lwext4) (BSD-2-Clause, pure C) in
  the guest at a pinned commit, and `tests/lwext4_cross_validate.rs`
  compares it with this crate in both directions: every kernel-made
  fixture lwext4's feature set covers, a tree this crate writes, and a
  tree lwext4 writes — on names, permission bits, sizes, symlink targets
  and SHA-256 of contents. The fixtures lwext4 does not implement
  (`inline_data`, `large_dir`, `metadata_csum_seed`, a partition table)
  are asserted to be REFUSED rather than left out, and a fixture in
  neither list fails the suite. `chore test:lwext4` is the tier;
  `scripts/test-floor.sh` gives it an executed-test floor, since a tier
  of one test binary that stops being selected would otherwise pass
  having run nothing.

- `META_BG` volumes mount and take writes. Their group descriptors are
  found per meta group (`Superblock::descriptor_location`, the kernel's
  `descriptor_loc`), and waking a `BLOCK_UNINIT` group reserves the
  descriptor block or backup at its head. `mke2fs` enables the feature on
  large volumes, which were refused at mount.
- `BIGALLOC` volumes mount and read. The refusal assumed clusters replace
  blocks as the group stride; they do not, and only the bitmaps and the
  descriptors' free counts are in clusters. Writes stay refused (the
  feature is not maintained), and `fsck::audit` and `verify::verify`
  refuse the volume by name.
- A volume with the `ENCRYPT` feature mounts read-only. The bit means some
  inode may be encrypted, and the volume was refused whole. Reading an
  encrypted file or symlink, and looking up or listing names in an
  encrypted directory, now fail with an error naming encryption
  (`file_io::refuse_encrypted`); everything else reads as usual. Writable
  mounts stay refused, since no write path checks the flag.
- `Filesystem::mount_with_cache(dev, blocks)` mounts with a chosen buffer
  cache capacity, and `DEFAULT_CACHE_BLOCKS` (256) is what `mount` uses;
  zero keeps no clean blocks. `tests/read_path_cost.rs` measures mount,
  walk, stat and read in device calls at no cache, the default and four
  times it, and `docs/read-path-cost.md` records the figures.

- `XattrEntry::new` and `XattrEntry::in_ea_inode`, the constructors a
  caller now needs (see Breaking).
- **A mount takes its wall time and inode generations from a provider.**
  `runtime::Runtime` (`now_unix_seconds`, `next_inode_generation`),
  `runtime::SystemRuntime` as the default, and
  `Filesystem::mount_with_runtime` to supply another (#144).
- **Checked journal recovery on an owned device.**
  `Filesystem::mount_recovering` opens a writable, exclusively owned device
  and replays a plain JBD2 journal the way the kernel does;
  `Filesystem::finish` releases it and reports flush errors, `flush` flushes
  without releasing, and `fresh_read` discards checkpointed read caches before
  a physical readback. `BlockDevice::invalidate_cache` backs the last; it has
  a default, so existing implementors are unaffected (#146).
- `file_mut::plan_truncate_shrink_deep`: truncate shrinks, and removes, an
  extent tree deeper than the inode (#169).
- **`chore check:semver`**, and the `semver` CI job under `ci-ok`: the public
  API is compared with the newest am-fs-ext4 on crates.io by
  cargo-semver-checks, and a break that Cargo.toml's version does not declare
  fails the pull request (#120).

### Changed

- **The test contract is chore tasks, and the first consumer of
  [fs-linux-test-harness](https://github.com/antimatter-studios/fs-linux-test-harness).**
  `chore tools` verifies what the HOST needs, `chore fixtures` builds the
  kernel-made `test-disks/*.img` in the harness VM, `chore test:unit` runs
  what needs no tool, fixture or VM, `chore test:images` what reads a
  fixture but needs no VM, `chore test:oracle` the e2fsprogs oracles,
  `chore test:kernel` the kernel oracles, `chore test:lwext4` the lwext4
  cross-validation, `chore test:vm` the whole suite
  inside the guest, and `chore test` everything exactly as CI does
  (`unit`, `fixtures`, `test`, `test-arm64`, `suite-in-vm`, and the
  `ci-ok` gate). `chore siblings` checks out `../rust-fs-core` and the
  harness at their pinned refs.
- **THE ORACLE TOOLS RUN IN THE HARNESS VM, NEVER ON THE HOST.**
  `e2fsck`, `debugfs`, `dumpe2fs`, `tune2fs` and `mke2fs` are no longer
  installed on any development machine or CI runner: e2fsprogs on a
  workstation is whatever that machine has — a keg-only Homebrew formula
  on a Mac, a distribution build on Linux, a different version per
  developer — and an oracle whose answer depends on which laptop asked is
  not an oracle. `scripts/vm-setup.sh` installs them in one Debian guest,
  `fs_ext4_test_support::oracle(tool)` is the only way a test reaches one
  (it returns the tool's own `Output`: same exit status, same streams),
  and `tests/test_contract.rs` fails the suite if a test spawns one
  itself, drives the VM itself, or mounts anything on the host. A Mac now
  needs no e2fsprogs at all. `chore tools` checks ripgrep and the VM host
  requirements instead.
  The VM is booted once per run — the first oracle call brings it up, and
  every later call rides one multiplexed SSH connection, about 0.1 s per
  tool invocation — and the chore reaper stops it when the invocation
  ends.
- **We run the Linux tests on Linux.** On a Linux host `chore test`
  compiles and runs the suite natively as before; on macOS it runs
  `chore test:vm`, which builds and runs the same sources inside the
  guest (`scripts/guest-suite.sh`, the harness's `[test] guest_command`,
  with the toolchain `rust-toolchain.toml` pins installed in the VM and a
  build directory on the VM's own disk so later runs are incremental). CI
  exercises that path on every pull request (`suite-in-vm`).
- **Scratch files live inside the repository** (`./tmp`), on every machine
  and on CI alike, because the guest sees this repository and nothing else
  of the host: an image under `/tmp` or `$RUNNER_TEMP` does not exist for
  the tool asked to read it. `FS_EXT4_TEST_TMPDIR` still names an exact
  directory and is now refused when it is outside the repository;
  `FS_EXT4_TEST_TMP_BASE` is gone.
- **No test skips.** Every test that returned early when a fixture or an
  e2fsprogs tool was missing now fails naming the task that provides it
  (`fs_ext4_test_support::fixture`, `oracle`, `assert_e2fsck_clean`, and
  the guest-kernel helpers).
- The `validate-mkfs-bin` CI job (and release.yml's `validate-fsck`) is now
  `tests/mkfs_bin_fsck_oracle.rs`, so it runs locally too.
- `tests/lwext4_cross_validate.rs` is one `#[ignore]`d test that fails when
  run, rather than two that printed SKIP and passed; the comparison is still
  unwritten (#99).

### Added

- **Kernel oracles: the driver writes, and the REAL KERNEL reads back.**
  `tests/kernel_readback.rs` (Rust API) and `tests/kernel_readback_capi.rs`
  (C ABI) build a tree on a fresh volume — directories, a multi-megabyte
  file written in several unaligned pieces, a short and a long symlink,
  xattrs, a POSIX ACL, a rename, an unlink, a truncate — then loop-mount
  the image read-only inside the harness VM and compare every name, type,
  mode, size, symlink target, xattr, ACL and SHA-256 against what was
  written, in ONE guest call. `this_driver_reads_back_what_the_kernel_wrote`
  goes the other way (the kernel writes into our image; the C ABI reads it
  back), and `a_flipped_data_byte_fails_the_comparison_and_not_e2fsck`
  flips one byte of file data and proves `e2fsck -fn` still calls the
  volume clean while the readback catches it — the comparison can fail, so
  its passing means something. `chore test:kernel`.
  They found two real defects in the tests' own understanding of the
  format, which is what an independent oracle is for: an ext4 ACL is
  version 1 (not the userspace xattr format's 2) and its entries without
  an id are four bytes, not eight — the kernel refuses either mistake on a
  volume `e2fsck` calls clean.
- `tests/oracle_debugfs.rs`: the driver writes (Rust API and C ABI), and
  `debugfs` reads every byte back (`dump` + compare) and the metadata
  (`stat`, `ex`, `icheck`, `ncheck`, `logdump`), with `e2fsck -fn` for
  consistency; files placed by `mke2fs -d` read identically through the
  driver; and a flipped data byte is shown to pass e2fsck and fail the
  debugfs comparison.

### Removed

- **The two FreeBSD cross-validation mechanisms (#269).**
  `tests/vagrant/freebsd/`, `tests/qemu/freebsd/`,
  `scripts/cross-validate-lwext4.sh` and the shell test that held the
  first two to failing closed (`tests/scripts/freebsd-manifests-fail-closed.sh`)
  are gone. Neither mechanism ever ran, and lwext4 is a portable C
  library: cross-validating against it needs a machine, not a BSD, and
  this repository already has one — the harness guest (see Added).
- `scripts/vm.sh`, `scripts/vm-slot.sh`, `scripts/vm-e2fsck.sh` and
  `tests/vagrant/debian/` (the harness replaces them), their shell tests in
  `tests/scripts/`, and the fixture builders
  `test-disks/build-ext4-feature-images.sh` (the Alpine VM),
  `build-ext4-feature-images-native-linux.sh` (sudo on the host),
  `vm-architecture.sh`, `build-test-disks.sh` and `gen-test-disks.sh`.
  `test-disks/_vm-builder.sh` is now `test-disks/guest-build-images.sh`.

### Fixed

- **A punch that needs two tree blocks from a `BLOCK_UNINIT` group gets two
  blocks.** A punch splitting an extent in a tree of full leaves needs a new
  leaf and an index node above it, in one transaction. The one-block allocator
  it drew from did not see the group's uninit flag the first allocation had
  cleared in the transaction's buffer, rebuilt the bitmap from metadata and
  returned the leaf's block again: the index node was written over the leaf
  and named itself as its child, which `e2fsck` reports as a cyclic loop in
  the extent tree. It now plans through the buffer, uninit clears included,
  as `pwrite` does since #145. Every other in-transaction planner was audited;
  each plans once or commits between plans, and each is now run into an
  uninit group under `e2fsck` (#291).
- **A `sparse_super2` volume's backup superblocks are no longer written
  over.** `s_backup_bgs` was read from 0x274 instead of 0x24C, where the
  on-disk layout puts it, so it came back zero and no group but 0 was
  believed to carry a backup. Once allocation reached a still-uninit
  backup group, its implied bitmap left out the backup superblock and
  GDT, and file data could be written over them; the overhead figures
  were wrong for these volumes too. The unit fixture had written the same
  wrong offset, so it agreed with the parser; it now writes 0x24C, and
  `tests/sparse_super2_backup_bgs_oracle.rs` checks the field against
  `dumpe2fs -h` and a filled volume against `e2fsck -fn` through both the
  primary and the last group's backup superblock (#318).
- **Splitting a full htree leaf is one transaction (#302).** The new right
  leaf was appended first — allocated, mapped, written and the directory's
  size grown with raw device writes and a commit of its own — and the
  halved leaf and the parent's routing entry were committed after it. A
  crash or an I/O error between the two left an allocated, mapped block
  the index never referenced, which e2fsck reports as a damaged index, on
  journaled volumes too. The allocation, the extent insert, the inode's
  size, both leaves and the routing entry are now staged into one
  `BlockBuffer` and committed once, and so is the rest of the operation
  that needed the room: the new file's inode for a create, mknod or
  symlink, the link count for a link or mkdir, and the rest of a rename.
  Growing a directory with no index is one transaction the same way. Only
  dropping an index that cannot route another leaf still commits early.
  `tests/htree_split_write_cut.rs` cuts the writes of a splitting create
  after each index and requires `e2fsck -fn` to accept every remounted
  image; 29 of 46 cuts were rejected before.

- **A revoke block's record count is bounded by the block, not clamped to
  it.** Replay read records up to `min(r_count, block length)`, so on a
  CSUM_V2/V3 journal the four-byte `r_checksum` tail was read as a record,
  and without checksums a count past the block was silently cut short. A
  checksum that happened to match a logged block revoked a committed write.
  A count past the block, less its tail when the journal has checksums, is
  now `Corrupt`, as the kernel's `scan_revoke_records` and `e2fsck` refuse
  it (#301).
- **Moving a directory to another parent refuses a corrupt directory block
  instead of re-stamping it.** On a `metadata_csum` volume the rename
  rewrote `..` in the moved directory's first block and recomputed its tail
  checksum without verifying the old one, so a bad checksum was replaced by
  a valid one over the corrupted contents. Nothing earlier in the rename
  reads that block. It is now verified before the edit, as the indexed
  branch already was, and a mismatch is `BadChecksum` with the block left
  untouched (#322).
- **A hole can be punched in a file whose extent tree is deeper than the
  inode.** Punching wrote what survived back into the inode's four inline
  entries and freed every node below, so a punch leaving more than four
  extents was refused with `Corrupt("surviving entries exceed inline-root
  capacity")` — which is every punch on a large file, the case a punch is
  for. `extent_mut::plan_repack_tree` now packs the survivors into full
  leaves and index levels over the blocks the file already holds: a punch's
  survivors are a subset of its entries, so the layout never needs more
  blocks than the tree has, and nothing is allocated inside an operation
  whose job is to free — except for the one case that needs it: a punch inside
  a single extent leaves a head and a tail where there was one record, so on a
  tree already packed full the layout is one block short and allocates it, in
  the same transaction. The nodes the layout no longer needs go back with the
  data blocks (#258).
- **A hole below the first entry of a deep extent tree reads as zeros.** The
  index descent kept the last entry at or below the block it was mapping and
  refused when there was none, which is every block before the first one a
  sparse file holds. An ordinary file whose data starts past block 0 was
  therefore unreadable at its leading hole, and the error called the extent
  tree corrupt on a volume `e2fsck` accepts. The descent now falls back to
  the first index entry, as `ext4_ext_binsearch_idx` does, and the leaf below
  it reports the hole (#260).
- **An insert in front of a leaf corrects the keys above it.** Every index
  entry holds the first logical block of the child it names, and an extent
  inserted before a leaf's first entry moves it. The keys were left as they
  were, so `e2fsck` reported `Logical start N does not match logical start M
  at next level` — reachable as soon as a leading hole could be written at
  all. `plan_insert_extent_deep` now carries the correction up the path
  beside any split it is propagating, as `ext4_ext_correct_indexes` does
  (#260).
- A directory the kernel indexed takes creates and unlinks. When the kernel
  turns a directory into an htree it keeps the old dirent tail's bytes in
  the root's `dt_reserved`, so the root ends in what looks like a dirent
  tail. The driver verified it as one and refused every create as a bad
  directory block, and every unlink as a bad record length. Index blocks are
  now recognised as the kernel does, by position and first record, and
  verified as index blocks (#233).
- Truncate refuses anything but a regular file. `apply_truncate_grow` and
  `apply_truncate_shrink` set the size of a directory, symlink or device
  node, leaving an inode e2fsck rejects. They now return `IsADirectory` for
  a directory and `InvalidArgument` otherwise, as the kernel does (#253).
- Replacing a file's content keeps its external xattr block in `i_blocks`.
  The count was set to the new data blocks alone, one block short for any
  file with an xattr block, on both the extent-mapped and block-mapped
  paths (#251).
- A write into a preallocated range lands in the preallocated blocks.
  `apply_pwrite` took an uninitialized extent, which reads as zeros, for a
  hole, and was refused inserting a fresh extent over it as a corrupt
  extent tree. The written range is now marked initialized
  (`extent_mut::plan_initialize_range`), and its blocks are written in
  full, so none of the preallocation's old contents can be read. A
  preallocation whose split doesn't fit the inode's inline root is refused
  as unsupported (#240).
- Journal transactions carry the checksums their journal declares. On a
  `metadata_csum` filesystem every tag, descriptor, revoke and commit
  checksum was written as zero, so Linux's recovery stopped at the commit
  and a crash mid-operation was never repaired. A CSUM_V3 tag is also 16
  bytes without 64-bit block numbers, as the kernel reads it.
- Replay believes only a committed transaction whose checksums hold. A
  torn commit ends the log, as does a damaged descriptor with no valid
  commit after it; a damaged descriptor, data or revoke block in a
  committed transaction refuses the replay. The tag after one carrying a
  UUID is read where it is, and a journal declaring `ASYNC_COMMIT`,
  `FAST_COMMIT`, an unknown incompat feature, CSUM_V2 with CSUM_V3 or a
  checksum type other than crc32c, or a block size other than the
  filesystem's, is refused.
- A commit sets `needs_recovery` before its journal goes live and clears
  it once the journal is clean again, including in a superblock block the
  transaction journals. Linux replays only a filesystem carrying that
  flag and wipes the journal of one without it, so a crash mid-commit was
  discarded rather than recovered.
- Replay marks the journal clean and clears `needs_recovery` once the
  writes are on disk. It left both set, so every mount replayed the same
  log again and `e2fsck` reported a journal still holding data.
- A read-only mount of a filesystem with a dirty journal reads the
  committed state. It skipped the journal and reported the superseded
  metadata as fact; it now replays the journal into the buffer cache,
  writing nothing, and re-reads the superblock and group descriptors
  through it. A journal the replay refuses now fails the mount rather than
  being read past.
- An ext3 volume from `mkfs` passes `e2fsck`. Its journal inode had mode 0,
  which `e2fsck` and the kernel take for no journal at all ("Superblock has
  an invalid journal (inode 8)").
- A directory or long symlink created on ext2 or ext3 is block-mapped rather
  than extent-mapped, which `e2fsck` called corrupt on a volume without
  extents. An ext2/ext3 directory also grows past its first block now,
  through its direct and single-indirect pointers; it refused before.
- Waking a `BLOCK_UNINIT` group sets the bitmap bits past the group's last
  block, as the kernel does. `e2fsck` reported "Padding at end of block
  bitmap is not set" on any volume whose groups are smaller than a bitmap
  block covers.
- The group descriptor table is located after the superblock's block
  rather than after `s_first_data_block`. They differ on a 1 KiB bigalloc
  volume, whose groups start at block 0.
- A create into a full htree leaf splits the leaf and routes the new half
  from the index, as the kernel does. The directory's whole index was
  dropped instead, leaving every other implementation a linear scan until
  `e2fsck -D`. The index is still dropped when the block routing the leaf
  is full, since interior nodes are not split.
- The dx entry planner (`htree_mut::plan_insert_dx_entry_*`) lays the entry
  array out where the kernel does, starting at the count/limit pair. It was
  one hash/block pair out of step.
- Writing an xattr block sets `COMPAT_EXT_ATTR` when the volume lacks it, as
  the kernel does. This crate's `mkfs` does not set the feature, and
  `e2fsck` clears every xattr block on a volume without it.
- Freeing a file frees every block its extent tree holds. Unlink,
  replace-content, rename-over and orphan release freed blocks only when
  `i_size > 0`, so an empty file with a `KEEP_SIZE` preallocation kept its
  blocks with nothing pointing at them. Punch-hole and rmdir freed only
  data extents, so a tree deeper than the inline root left its index and
  leaf blocks allocated, and counted in `i_blocks` for a punch. An unlink
  of such a file was refused. These paths now free what
  `extent::collect_all_with_nodes` reads from the tree (#242).
- An external xattr block shared by several inodes is edited as shared.
  The kernel keeps one block for every inode with an identical attribute
  set and counts them in `h_refcount`. `apply_setxattr` rewrote it in
  place and reset the count to one, changing the other inodes' attributes
  too. `apply_removexattr` freed it while others still pointed at it. Unlink,
  rmdir, rename-over and orphan release never released it at all. An edit
  now gives the inode its own copy, and every release drops one reference,
  freeing the block only at the last (#245).
- Unlinking a block-mapped (ext2/ext3-style) file frees its blocks.
  `apply_unlink` freed blocks only for extent-mapped files, so every data
  and indirect block of a block-mapped file stayed allocated. Rewriting one
  with `apply_replace_file_content` now goes through the journaled buffer
  too. It wrote the block bitmap directly and without restamping its
  checksum, which e2fsck reported on a volume with `metadata_csum` (#249).
- A name holding a NUL byte is refused. Names are taken as `&str`, which
  can hold one, and create, mkdir, mknod, symlink, link and rename filed it
  into the entry; e2fsck reports it as an illegal character. The kernel
  never writes one, and `split_parent_and_base`, which every one of them
  uses, now refuses it (#247).

## [0.5.1] — 2026-09-06

### Fixed

- A journal entry that names a block outside the filesystem is refused
  rather than replayed. Replay writes where the entry says, so a
  crafted journal could write anywhere on the device.
- The superblock fields a mount sizes itself from are bounded, so an
  image cannot decide how much memory a mount spends.
- A directory's declared size no longer drives an unbounded scan.
- The allocator and the journal writer may not step outside the
  filesystem.
- A group descriptor's pointers are read as block numbers in this
  filesystem rather than as offsets, which is what they are.

## [0.5.0] — 2026-09-04

### Breaking

- **The C attribute struct's timestamps widen to `int64_t`.**
  `fs_ext4_attr_t`'s `atime`/`mtime`/`ctime`/`crtime` were `uint32_t`.
  ext4's on-disk base field is a *signed* 32-bit value which the
  matching `*_extra` field extends by two further bits, so the real
  range is roughly 1901..2446 — a `uint32_t` truncated everything past
  2038 and turned every pre-1970 date into a far-future one.
  **This moves every field after them in the struct: consumers must
  recompile, not merely relink.** `include/fs_ext4.h` is updated to
  match.

- **`fs_ext4_utimens` takes `int64_t` seconds, and the "leave unchanged"
  sentinel moves.** The setter took `uint32_t`, which could not name a
  pre-1970 time and — worse — wrote a post-2038 time into the signed base
  field with the epoch bits left zero. With the reader above now decoding
  those bits, setting an mtime of 2046 and reading it back gave 1909,
  from a call that returned success. `UINT32_MAX` was the sentinel meaning
  "leave this timestamp alone"; under a signed 64-bit parameter it is an
  ordinary date in 2106, so the sentinel is now `FS_EXT4_TIME_OMIT`
  (`INT64_MIN`). A time outside what the format can store, or one needing
  the epoch bits on a 128-byte inode with no `*_extra` field, is refused
  with `EINVAL` before any byte is written. Callers must update both the
  argument types and the sentinel.

  The header declares the sentinel as `static const int64_t`, not a
  `#define`: Swift's C importer accepts simple literal macros only and
  silently drops `#define FS_EXT4_TIME_OMIT INT64_MIN`, leaving the symbol
  absent on the Swift side with no diagnostic.

- **`CachingDevice` is no longer part of this crate's public API.** It was
  removed while deleting what a dead-code `allow` was hiding; the type lives in
  `am-fs-core` now, where the other drivers were already getting it. The
  functionality moved rather than disappearing, but
  `fs_ext4::block_io::CachingDevice` no longer resolves, which is why this is a
  minor rather than a patch — for a `0.x` crate cargo treats the minor as the
  compatibility boundary. Nothing outside this repo referenced it.

### Added

- **`mkfs.ext4` is published as a downloadable binary.** A release job builds
  the formatter, renames it from the cargo target `mkfs_ext4` to the dotted name
  the format's convention uses — cargo refuses a dot in a target name — and
  attaches a per-platform archive to the release, so a package manager can
  install it without a Rust toolchain. The rename happens at package time rather
  than in a downstream formula, so anyone downloading the archive directly gets
  the real name.

### Fixes

Four of these come from `docs/format-conformance-gaps.md`, which
catalogues places the reader accepted a filesystem it could not read
correctly. They share a failure mode: the mount succeeded and the
wrongness appeared later as data rather than as an error.

- **A bigalloc filesystem is refused rather than misread.** bigalloc
  moves the allocation unit from the block to the cluster, so a reader
  assuming they are the same computes every block-group offset wrong.
  Measured: with the refusal disabled, such an image fails with
  `BadChecksum { what: "block group descriptor" }` — the misreading
  caught in the act, and caught only because `metadata_csum` happened
  to be on. Without it the wrong read returns silently.

  The refusal is a named list, not "anything unrecognised": an unknown
  RO_COMPAT bit must still mount, which is the compatibility model.
  (The mask that previously claimed to control this was dead code —
  the check ignored its argument entirely.)

- **Timestamps apply the epoch-extension bits and are read as signed.**
  The `*_extra` fields were read only for their nanosecond half by
  `>> 2`; the seconds extension lives in exactly the two bits that
  discards. Every timestamp past 2038 came back 136 years early, and
  every pre-1970 date came back as one in 2106.

- **A missing or short inline-data spill is corruption, not an empty
  tail.** An inline file over 60 bytes stores its remainder in the
  `system.data` xattr. If that xattr was absent the read silently
  returned only the first 60 bytes — for a file whose size field still
  claimed the full length, so nothing downstream could detect the
  truncation.

- **A writable mount of an MMP filesystem is refused.** Multi-Mount
  Protection stops two hosts writing to one filesystem at once, and is
  not implemented. Read-only mounts are unaffected and still work,
  which is what recovering data from a disk another machine has open
  requires.

- **A truncated inode read fails closed.** The checksum verifier accepted a
  short read as a pass; it now refuses one, because a read that did not return
  the bytes is not a checksum that matched.

### Internal

- Dependencies move to `am-fs-core` 0.2.4.
- `tests/feature_matrix.rs` builds a filesystem with each feature via
  `mke2fs` and asserts the crate either reads it correctly or refuses
  it. Every gap fixed above was found by reading code, not by a test
  failing; this is the test that would have caught them.

## [0.4.1] — 2026-08-29

### Fixes

- **`mkfs.ext4 -c` no longer swallows the device path** — `-c` asks the standard
  formatter for a bad-block scan and takes no argument, but it was listed among
  the flags that consume the token after them. `mkfs.ext4 -c disk.img` therefore
  read the image path as the value of `-c` and then failed with "missing
  positional `<device>` argument" about the path it had just consumed.

- **`mkfs.ext4 -b` rejects an unusable block size before opening the device** —
  the power-of-two / 1 KiB..64 KiB rule was enforced only inside the formatter,
  which runs after the device is opened read-write and after the tool has
  printed that it is formatting it, so `-b 3000` looked like a format that had
  failed part way through rather than a rejected argument. The rule now lives in
  one predicate (`mkfs::is_valid_block_size`) that both the formatter and the
  CLI call.

- **`mkfs.ext4 -q` no longer depends on where it appears** — `quiet` was read
  while the loop that sets it was still running, so `-q -m 1` was silent and
  `-m 1 -q` was not.

### Testing

- The `mkfs_e2fsck_oracle` suite gains multi-group cases (320 MiB / three
  groups with a short final group, and 640 MiB / five groups so a backup lands
  in group 3 under the sparse-super powers-of-3 rule), and CI runs `fsck.ext4`
  against both — including through their backup superblocks with `-b`. The
  multi-group formatter path had previously been validated only by this crate's
  own reader. It passes.

## [0.4.0] — 2026-06-30

### Features

- **Multiple block groups (ext4)** — `format_filesystem` /
  `format_filesystem_with_flavor` formats ext4 volumes spanning more than one
  block group: a full block-group descriptor table plus per-group block and
  inode bitmaps and inode tables. Superblock/GDT backups are written in the
  `RO_COMPAT_SPARSE_SUPER` groups (0, 1, and further powers of 3/5/7), so that a
  damaged primary superblock is recoverable via `e2fsck -b`. Only the filesystem
  metadata is written, so large volumes format quickly. ext2/ext3 remain
  single-group entities.

### Fixes

- **Metadata-checksum & free-count coherency on write** — bitmap, inode, and
  journal-superblock checksums are now recomputed when the underlying metadata
  changes; `bg_itable_unused` is maintained on inode allocation; superblock
  free-block / free-inode counts stay coherent across consecutive allocate/free
  ops (`patch_sb_counters` was re-reading the immutable mount-time snapshot and
  clobbering deltas). Fixes `e2fsck` "does not match checksum" / "free … count
  wrong" reports on written images.

- **Write-path hardening (dir / xattr / pwrite)** — `apply_rmdir` clears the
  freed directory inode; external xattr blocks compute correct entry/block
  hashes; `removexattr`'s external-block free is journaled (crash-safe); large
  pwrites are split into per-transaction chunks so they can't overflow a single
  journal descriptor block. Directory growth refreshes the block-bitmap
  checksum alongside the BGD + SB counter updates in one cache-coherent commit.

- **mkfs 1 KiB-block & ext3 correctness** — the group-0 block bitmap is indexed
  in bit space (accounting for `first_data_block = 1` on 1 KiB blocks), fixing
  an off-by-one tail and the missing trailing pad bit; the ext3 journal's
  indirect-map blocks are counted as used. 1 KiB and ext3 images now pass the
  in-process `fsck::audit`; 2 KiB / 4 KiB output is byte-identical to before.
  New `mkfs_e2fsck_oracle` / `mkfs_ext3_oracle` oracle test harnesses across
  block sizes and flavors.

## [0.3.3] — 2026-06-21

- Renamed the published crate `fs-ext4` → `am-fs-ext4` (consistency with the
  `am-*` sister crates); first pipeline-driven release under the new name. The
  `[lib]` name stays `fs_ext4`, so the C ABI / `.a` / downstream linking are
  unchanged.

## [0.3.2] — 2026-06-09

- README accuracy pass (dependencies, test counts, version).

## [0.3.1] — 2026-06-09

- Release pipeline: drop `--locked` from `cargo publish` so it can rewrite the
  `am-fs-core` path dependency into its registry equivalent in the packaged
  lockfile.

## [0.3.0] — 2026-05-30

### Features

- **Directory extent trees beyond depth 1** — `extend_dir_and_add_entry` now
  handles directories whose extent trees have reached depth ≥ 2 (previously
  returned a hard error). Uses the same `plan_insert_extent_deep` machinery
  as file writes; allocates index-node blocks on demand via
  `plan_block_allocation`.

- **Depth-1 directory leaf-full fallback** — When the single leaf block in a
  depth-1 directory extent tree fills up, the driver now falls back to the
  deep path (adds a sibling leaf or promotes to depth 2) instead of returning
  an error.

- **Full Unicode NFD + case fold for CASEFOLD directories** — `fold_name` now
  applies proper Unicode NFD decomposition (`unicode-normalization` crate)
  followed by Unicode full case fold (`caseless` crate). Previously only
  ASCII A–Z → a–z was folded, causing lookups for non-ASCII names (e.g. 'ñ'
  vs 'Ñ', 'ß' vs 'ss') in CASEFOLD-enabled directories to miss the htree
  entry and silently fail to find the file.

- **`fs_ext4_mknod`** — New C API function to create special files: FIFOs,
  Unix domain sockets, character devices, and block devices. Mirrors POSIX
  `mknod(2)`; accepts `mode` with type bits (S_IFIFO=0x1000, S_IFSOCK=0xC000,
  S_IFCHR=0x2000, S_IFBLK=0x6000) plus permission bits, and `major`/`minor`
  device numbers (0 for FIFOs and sockets). Device numbers encoded using both
  old format (i_block[0]) and new format (i_block[1]) for maximum
  compatibility.

- **`fs_ext4_set_flags`** — New C API function to update the `i_flags` word
  (FS_IOC_SETFLAGS) on any path. Bumps i_ctime on write.

- **`fs_ext4_attr_t` extended** — Seven new fields appended at the end of the
  struct (ABI-compatible for consumers that zero-initialise):
  - `atime_nsec`, `mtime_nsec`, `ctime_nsec`, `crtime_nsec` — sub-second
    nanoseconds from the extra timestamp words; zero on ext2/ext3 inodes.
  - `inode_flags` — on-disk e2_flags (FS_IOC_GETFLAGS convention); callers
    can detect IMMUTABLE, NODUMP, APPEND_ONLY, CASEFOLD, etc.
  - `generation` — `i_generation` for NFS stale-handle detection.
  - `blocks_512` — `i_blocks` in 512-byte units (matches `st_blocks`).

## [0.2.1] — 2026-05-20

### Fixes

- **`fs_ext4_create` now writes `i_crtime`** at offset 0x90 across
  all four inode builders (regular file, directory, fast symlink,
  slow symlink). Previously the field was left at zero on freshly
  created inodes, surfacing on Darwin as `st_birthtime` = 0 and
  Finder showing "1 January 1970" as the file's "Created" date
  even though atime / ctime / mtime were set correctly. Gated on
  `inode_size >= 0x94` so ext2 / ext3 128-byte inodes (which lack
  the extra section) are left alone.

### Tests

- New `tests/capi_create.rs::create_sets_timestamps_to_now`
  regression test brackets all four timestamp fields against
  `SystemTime::now()` either side of `fs_ext4_create`. Fails on
  the pre-fix code.

### Release pipeline

- `cargo publish --locked` in the release workflow. Earlier
  publish attempts failed because `Cargo.lock` got modified
  during the implicit publish-verify build and the working tree
  went dirty; the lazy `--allow-dirty` would have hidden any real
  drift. With `--locked`, cargo refuses to update the lockfile
  during publish, so the version of every transitive dep
  committed at the tag is the version that ships. Discipline:
  any Cargo.toml `version` bump must be paired with
  `cargo update --workspace` + a lockfile commit in the same
  release prep.

## [0.2.0] — TODO

(Pre-existing release. Changelog entry not authored at the time;
fill in if it ever becomes useful.)

## [0.1.4] — TODO

(Pre-existing release. Changelog entry not authored at the time;
fill in if it ever becomes useful.)

## [0.1.3] — 2026-04-22

### Fixes

- `tests/capi_basic.rs::volume_info_flags_dirty_image` is now
  formatted per rustfmt. Shipping 0.1.2 with that line unformatted
  broke `cargo fmt --check` in CI, which in turn blocked clippy and
  test. No ABI / behaviour change.

### Tooling

- Pre-commit hook (`.githooks/pre-commit`) runs the fast CI subset —
  `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings`
  — so the same class of miss can't slip through again. Enable with
  `./scripts/install-hooks.sh`.

## [0.1.2] — 2026-04-20

### ABI additions

- `fs_ext4_volume_info_t` gained a trailing `uint8_t mounted_dirty`
  field. `1` means the filesystem was not cleanly unmounted last time
  it was used (captured from the on-disk `s_state` superblock field at
  mount time); `0` means clean. Callers can surface this to the user
  and run fsck / journal replay before permitting writes. Existing
  consumers compiled against 0.1.1 remain source-compatible — the new
  field is appended and initialised to 0 via the existing struct-zero
  path in `fs_ext4_get_volume_info`.

### Rust API additions

- `Superblock` now parses `s_state` into a new `state: u16` field and
  exposes `Superblock::is_clean()`. New constants `EXT4_VALID_FS` and
  `EXT4_ERROR_FS` mirror the kernel's `s_state` bits.

### Tests

- `tests/capi_basic.rs::volume_info_flags_dirty_image` flips `s_state`
  on a copy of the no-csum fixture and asserts the ABI surfaces
  `mounted_dirty == 1`. `volume_info_reports_expected_fields` now also
  asserts `mounted_dirty == 0` for the freshly-built clean fixture.

## [0.1.1] — 2026-04-20

### Docs / packaging

- README fully rewritten. New sections: origins, a concrete
  capability matrix contrasting ext4rs with its research references
  (`yuoo655/ext4_rs` and `lwext4`) to justify this crate's existence
  as an independent FFI-first implementation, and a plain-English
  at-your-own-risk disclaimer restating the MIT no-warranty clause.
- Framing neutralised: crate is described as a general-purpose FFI
  ext4 driver; no more `Swift` / `FSKit`-specific language in the
  API description.
- `Cargo.toml` description updated to match (`FFI from C/C++/Go/etc.`
  instead of `Swift/C/Go/etc.`) and `version` bumped to `0.1.1`.

### Safety / robustness

- Mount path no longer panics on malformed images. Superblock parse
  rejects `blocks_per_group == 0`, `inodes_per_group == 0`,
  `inode_size == 0`, `inode_size > block_size`, and `log_block_size`
  above the spec-sane maximum. Block/inode arithmetic in
  `fs::read_block`, `fs::read_inode_raw`, `bgd::locate_inode`, and
  `extent::lookup` now uses `checked_mul`/`checked_add`; overflows
  surface as structured `Error::Corrupt` instead of silent wraps or
  div-by-zero panics.
- New `tests/fuzz_smoke.rs` harness: truncated / zero-filled /
  all-ones images, an xorshift PRNG seed fan, single-byte flips at
  sampled superblock+BGDT+inode-table+dir-block offsets, direct
  random-bytes feeding into `dir::parse_block` and the extent
  parsers, and an exhaustive-single-bit-flip sweep of the
  superblock sector. Every combination must either succeed or
  return a structured `Err` — never panic.

### Features

- `Filesystem::audit(max_dirs, max_entries_per_dir)` — read-only
  fsck-style link-count audit (see `src/fsck.rs`). Returns an
  `AuditReport` listing `LinkCountTooLow` / `LinkCountTooHigh` /
  `DanglingEntry` / `WrongDotDot` / `BogusEntry` anomalies. Pure
  diagnostic: never writes. Bounded work so pathological images
  can't hang the caller.
- `CachingDevice` — LRU read cache decorator for any
  `Arc<dyn BlockDevice>`. Caches only block-aligned, block-sized
  reads (hot paths: `fs::read_block`, extent index blocks, bitmap
  blocks); passes arbitrary-offset reads through. Writes
  invalidate overlapping entries. Opt-in — existing callers see no
  behaviour change. Primary target is the FSKit `CallbackDevice`
  path where repeated reads of the same inode-table / bgd blocks
  dominate directory walks.

### Performance

- `alloc::find_first_free` — scan the free-block bitmap a `u64` at
  a time once aligned to an 8-byte word. Skips full words in a
  single branch; uses `trailing_ones` to locate the first zero
  within a non-full word. 8–16× faster than the previous per-bit
  loop on sparse bitmaps.

### Build / CI

- Test-disk fixtures now regenerate from scratch on any host with
  `qemu-system-x86_64` + `libarchive-tools` (for `bsdtar`'s
  ISO9660 writer). Drop-in `bash test-disks/build-ext4-feature-images.sh`
  boots a short-lived Alpine Linux VM, runs ext4 formatter + friends
  inside, writes the image matrix out via 9p. Replaces the earlier
  docker-based path so macOS dev hosts don't need Docker Desktop.
  CI (`ubuntu-latest`) runs this before `cargo test`.

## [0.1.0] — 2026-04-18

First public release. Extracted from the internal ext4-fskit research
repo into a standalone crate.

### C ABI — `fs_ext4_*`

- Lifecycle: `fs_ext4_mount`, `fs_ext4_mount_with_callbacks`,
  `fs_ext4_mount_rw`, `fs_ext4_umount`, `fs_ext4_get_volume_info`.
- Metadata: `fs_ext4_stat`, `fs_ext4_last_error`, `fs_ext4_last_errno`.
- Directories: `fs_ext4_dir_open`, `fs_ext4_dir_next`, `fs_ext4_dir_close`.
- Files: `fs_ext4_read_file`, `fs_ext4_readlink`, `fs_ext4_listxattr`,
  `fs_ext4_getxattr`.
- Write ops: `fs_ext4_create`, `fs_ext4_unlink`, `fs_ext4_mkdir`,
  `fs_ext4_rmdir`, `fs_ext4_rename`, `fs_ext4_link`, `fs_ext4_write_file`,
  `fs_ext4_truncate`.

### Driver features

- Multi-level extent tree promotion (depth 0 → depth 1) in
  `extent_mut`, with `Checksummer::patch_extent_tail` so newly
  built leaf blocks carry a valid `ext4_extent_tail.et_checksum`.

### Build / CI

- `cargo fmt` + `cargo clippy --all-targets -- -D warnings` + `cargo
  test --release` on `ubuntu-latest`.
- `CallbackDevice` fields use `ReadCb` / `WriteCb` / `FlushCb` type
  aliases instead of inline `Box<dyn Fn(...) + Send + Sync>`.

### Known gaps

- Multi-level extent tree mutation beyond depth 1 not implemented;
  very large / fragmented writes will fail loudly.
- Sparse grow via truncate not implemented.
- `setxattr`, `removexattr`, `chmod`, `chown`, `utimens` — not in the
  ABI; reads only for xattrs.
- Write path is unjournaled. `jbd2` replay works at mount for a
  cleanly-closed journal; live transactions are not yet wrapped.

### Origin

- Imported from `github.com/christhomas/ext4-fskit@aaa63cf`.
