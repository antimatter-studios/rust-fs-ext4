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
- `Filesystem::replay_journal_if_dirty` takes `&mut self` (#376): a replay
  that applied anything now reloads the mount's superblock and descriptors and
  reopens its journal writer, which a shared borrow cannot. The C entry point
  is unchanged.
- `hash::HTREE_EOF` is removed with the clean-room `src/hash.rs` (see
  Changed). The value it named, `0xFFFF_FFFE`, is the directory index's
  end-of-directory marker, and `hash::name_hash` never returns it.

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
- **`cfg->block_size` is honoured on the callback mounts** (#373). It was
  documented as the device's physical block size, but no mount path read
  it, so a host whose block resource accepts only sector-aligned I/O had
  its first request — the superblock, 1024 bytes at offset 1024 — refused
  and the mount returned NULL. With `block_size` greater than 1,
  `fs_ext4_mount_with_callbacks`, `fs_ext4_mount_rw_with_callbacks` and
  `fs_ext4_mount_rw_with_callbacks_lazy` now send the callbacks only
  requests whose offset and length are multiples of it: an unaligned read
  is widened into a bounce buffer, an unaligned write becomes a
  read-modify-write of its first and last sectors, and an aligned request
  passes straight through (`block_io::AlignedDevice`). A `block_size` that
  is not a power of two, does not divide `size_bytes`, or is larger than
  the filesystem block size is `EINVAL`. **A caller that already set it —
  512 is common — now receives 512-aligned requests**; 0 or 1 keeps the
  byte-granular requests. On `fs_ext4_mkfs` the field still means the
  filesystem block size. `tests/capi_callback_alignment.rs` mounts,
  writes and reads through a device that refuses unaligned I/O, and
  `tests/capi_callback_alignment_oracle.rs` has e2fsck check the result.

- **`Runtime::now_unix_seconds` returns `i64`, not `u32`.** The clock the
  driver stamps inodes from could not express a time past 2038 as ext4
  stores one — a signed 32-bit base extended by the epoch bits of each
  `*_extra` field — nor a time before 1970. An implementation of
  `Runtime` changes its return type; `SystemRuntime` callers change
  nothing.

### Added

- **The command-line tools are one multi-call binary, `rust-fs-ext4`**
  (#440), behind a new `cli` cargo feature (`cargo install am-fs-ext4
  --features cli`), so the static library gains no dependency from them.
  It dispatches on `argv[0]`: installed as `mkfs.ext4` it is the formatter,
  and `rust-fs-ext4 mkfs ...` is the same program under the one name
  nothing else on PATH can shadow. `--version` on every name prints
  `<tool> (am-fs-ext4) <version>`.
  - `mkfs.ext4` keeps every flag it accepted, the ignored standard ones
    included; `--size` is the new spelling of `--create-size`, which stays
    as an alias. **It now prints a JSON report on stdout** (label, UUID,
    block size, block and inode counts, read back from the new superblock),
    where it printed nothing; `--text` keeps the old silence. Errors are
    `{"error": "...", "code": N}` on stderr, `N` being the exit status: 1
    for a failed run, and now **2** for a wrong command line, which was 1.
  - `rust-fs-ext4 doctor` resolves each tool's name on PATH, checks through
    `--version` that the program found is this one at this version, and
    names what wins and the fix (`brew unlink <formula>`, or which PATH
    entry to move) when it is not. JSON by default, `--text`, exit 1 when
    anything is missing, shadowed or stale.
  - A `cli` test tier, `chore test:cli`, tests the tools as installed:
    `chore cli:install` stages them in `tmp/cli/bin`, doctor runs first, and
    a missing or shadowed tool fails the tier naming the fix. A new `cli`
    CI job builds, stages and tests them on every pull request.
  - `fs.ext4 <target> <verb>` works inside an image or device without
    mounting it: `ls [path]` (JSON entries: name, type, size, mode, mtime,
    inode, and a symlink's target), `read <path> [-o FILE]` (raw bytes),
    `get [key]` / `info [key]` (the envelope: `fs`, `label`, `total_bytes`,
    `free_bytes`, `block_size`, `dirty`, and ext4's own fields under
    `ext4`). `--offset BYTES` reaches a partition inside a whole-disk
    image. `write <path>` creates or replaces a file with the bytes on
    stdin, read in full before the image is opened; `mkdir <path>` makes
    one directory. Both leave the volume marked clean. `set label` (#447)
    and `resize` exist and answer `not implemented` with status 3 until the
    library can do them.
  - `fsck.ext4 <target> [-n | -y | -p]` runs the library's audit (link
    counts, `..` entries, entry types, directory-block checksums, group and
    superblock free counts) and, with `-y`/`-p`, its repairs. Nothing is
    written without `-y` or `-p`. The exit status is fsck(8)'s: 0 clean, 1
    corrected, 4 left uncorrected, 8 operational error, 16 usage. A JSON
    report by default. A directory the audit cannot read is skipped rather
    than reported (#445), so such a volume is called clean until that is
    fixed.
  - Man pages (section 8 for `mkfs.ext4` and `fsck.ext4`, section 1 for
    `fs.ext4`, its subcommands and `rust-fs-ext4`) and zsh, bash and fish
    completions for every name are generated by the binary itself from its
    argument definitions (`rust-fs-ext4 generate man|completions SHARE`),
    so they cannot describe a flag the tools do not take. `chore
    cli:install` stages them in `tmp/cli/share`.
  - The `mkfs_ext4` target is unchanged for now; it is retired when the
    release packages the multi-call binary.
- **`FileDevice::open_path` and `FileDevice::open_path_rw`** take a
  `&Path`, so a target whose name is not UTF-8 is opened by its own bytes;
  the command-line tools open their target through them.
- **Content writes to inline-data files** (#428). pwrite, replace and
  truncate, through paths and through inode numbers, now write an inline
  file instead of refusing it with `Unsupported` (#383). A write whose
  result fits in the inode is made there — the first 60 bytes in `i_block`,
  the rest in `system.data`, as the kernel keeps them; one that outgrows it
  converts the file to an extent-mapped one, its bytes moved to a new
  block, `system.data` removed and `EXT4_INLINE_DATA_FL` cleared, in the
  same transaction as the write, so a crash leaves the old inline file or
  the written one. `e2fsck` judges the result clean and the kernel reads it
  back.
- **Directory mutations inside inline-data directories** (#428). create,
  mknod, mkdir, symlink, link, unlink, rename and rmdir, by path and by
  `(directory, name)`, now edit an inline directory instead of refusing it
  with `Unsupported` (#382). An entry is added in `i_block` or the
  `system.data` continuation, which is created at the size the inode holds
  when `i_block` is full, and removed where it is, as the kernel does, so
  the other entries keep their places; a moved inline directory's `..` is
  rewritten in `i_block`. A directory whose entries outgrow the inode is
  converted to a one-block directory holding them in the same transaction,
  and a non-empty one is still refused by rmdir. `e2fsck` judges the result
  clean and the kernel lists it.
- **`fs_ext4_flush` and `fs_ext4_fresh_read` in the C API** (#374). A host
  embedding the engine through C had no durability barrier on a live mount,
  so it could not tell "every change so far is on the device" from "the
  mount has been released", and unmounted after every mutation. Every
  mutating call already reaches its final on-disk location before it
  returns; `fs_ext4_flush` issues one more device flush and checks nothing
  is left in flight, returning -1 with `EIO` after an earlier failed write
  or journal operation. `fs_ext4_fresh_read` also drops the read caches for
  a host that changed the device underneath, and refuses with `EIO` on a
  read-only mount holding a journal replayed only into its cache, keeping
  that view. The header no longer names a `synchronize()` that never
  existed; `tests/capi_header_names_real_functions.rs` fails when it names
  a function it does not declare, or declares one the library does not
  export. `Filesystem::flush` now takes `&self`, holding the journal writer
  for the whole check, so a shared handle can flush while other calls run.

- **Inode-addressed entry points, for a host that holds handles rather
  than paths** (#372). A handle-based host names an item by its inode
  number and a mutation by a (directory, name) pair; a path is the wrong
  key for it, because a hard link gives one inode several and a directory
  rename changes every one beneath it. `Filesystem` gains `lookup_at`,
  `stat_ino`, `read_dir_ino`, `read_ino`, `read_link_ino`,
  `apply_{create,mkdir,mknod,symlink,unlink,rmdir}_at`, `apply_link_at`,
  `apply_rename_at` and `apply_{pwrite,truncate,chmod,chown,utimens}_ino`,
  taking an `InodeRef` (or a bare `u32`) and names as bytes, so a name
  that is not UTF-8 survives. They are the implementation: each path
  function now resolves its path and calls its inode twin, and
  `tests/inode_api.rs` holds the two to the same bytes on disk after every
  step of scripted and random operation sequences. An `InodeRef` carrying
  the generation it was read with is refused with the new `Error::Stale`
  (ESTALE) once its inode is freed or reused for another file; so is a
  number that never named a file. `tests/inode_api_e2fsck.rs` has e2fsck
  judge a volume written only through them.
- **The same entry points in the C ABI** (#372): `fs_ext4_lookup_at`,
  `fs_ext4_stat_ino`, `fs_ext4_dir_open_ino`, `fs_ext4_pread_ino`,
  `fs_ext4_readlink_ino` (the `fs_ext4_readlink` contract: length
  returned, NUL-terminated, ERANGE when too small),
  `fs_ext4_{create,mkdir,mknod,symlink,unlink,rmdir}_at`,
  `fs_ext4_link_at`, `fs_ext4_rename_at` and
  `fs_ext4_{pwrite,truncate,chmod,chown,utimens}_ino`. Every inode
  argument is followed by the generation read with it, or
  `FS_EXT4_GEN_ANY`; names are counted byte buffers; the calls that make
  an inode fill an optional `fs_ext4_attr_t` for it. The path exports and
  these share their read, list, readlink, truncate and argument checks,
  and `tests/capi_ino_api.rs` holds each to its path twin.
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
  found per meta group (`Superblock::descriptor_location`, the placement the
  format documentation's "Meta Block Groups" section describes), and waking a `BLOCK_UNINIT` group reserves the
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
### Fixed

- **Paths cross the C ABI as bytes, and a path naming nothing is never the
  root (#418).** Every path-taking entry point read its `const char *` as
  UTF-8 and answered `""` for one that did not decode or was longer than
  `PATH_MAX` — and `""` means the root here, so `fs_ext4_stat` reported the
  root's attributes and returned 0, and `fs_ext4_dir_open` listed the root.
  A caller handing back a non-UTF-8 name `fs_ext4_dir_next` had just listed
  walked in a circle. Paths are now the bytes up to the NUL, compared byte
  for byte against the entry names, which have no encoding: such a name is
  reachable by `stat`, `dir_open`, `read_file`, `readlink`, the xattr calls
  and every write call. A path naming no file is ENOENT; one over 4,096
  bytes is ENAMETOOLONG; a NULL path is EINVAL, as it already was.
  Source-compatible for every caller passing UTF-8. Extended-attribute
  names are not paths and are still text: one that is not UTF-8 is EINVAL.
  The Rust `&str` path API is unchanged and now wraps the byte resolution,
  which `Filesystem::lookup_path_bytes` and
  `path::lookup_bytes_with_csum` expose. `tests/non_utf8_names_oracle.rs`
  has `debugfs` file the names and `e2fsck` judge what was written through
  them.
- **`chore staticlib` no longer calls a stale or broken artifact up to
  date (#330).** `dist/include/fs_core.h` is a generated file, so deleting
  it rebuilds; `Cargo.lock` and rust-fs-core's manifest and sources are
  sources, so a lock-only bump or a core change rebuilds; and the build is
  `--locked`, so a manifest that no longer matches the lock fails instead
  of re-resolving. `tests/scripts/test-staticlib-freshness.sh` checks all
  three against the real task.

### Changed

- **`hash::name_hash` never returns the reserved major hash
  `0xFFFFFFFE`.** The directory index reserves that value as its
  end-of-directory marker, so a name whose major hash would be it now gets
  `0xFFFFFFFC`, as the BSD-licensed implementations of the format do. This
  affects roughly one name in 2^31. `debugfs dx_hash` prints the value
  before the remap; `tests/htree_hash_differential.rs` checks that it
  prints `0xFFFFFFFE` for such a name and that `name_hash` gives
  `0xFFFFFFFC`.
- **`src/hash.rs` is re-implemented clean-room** from RFC 1320 (MD4), the
  TEA paper and the kernel.org ext4 directory documentation, with
  `debugfs dx_hash` as the oracle. Apart from that remap its output is
  `debugfs`'s: `tests/htree_hash_differential.rs` compares 14,400 hashes
  (2,400 names across six seeds, in all six hash versions) with it. See
  `PROVENANCE.md`.
- **The provenance record and its remediation.** `PROVENANCE.md` records
  where the code comes from, the sources it may be written from, and the
  2026-09-29 audit against the Linux ext4 and JBD2 sources. Besides the
  clean-room `src/hash.rs` above, the inode timestamp helpers and five
  format-driven routines (the xattr entry and block hashes, the JBD2 tag
  sizes and checksum-declaration rules, descriptor placement and group-head
  size, directory block roles, and the `BLOCK_UNINIT` bitmap) are restated
  and checked against e2fsprogs and a Linux kernel in the harness VM;
  comments cite the format documentation or an oracle instead of kernel
  internals; and `chore check:provenance`, also a CI job, fails when the
  tree names kernel or e2fsprogs internals. Every earlier release on
  crates.io, 0.3.2 to 0.5.1, contains htree hash code derived from the
  Linux kernel, and 0.5.0 and 0.5.1 also quote one line of kernel C; the
  README's statement that no code derives from GPL, LGPL or AGPL source
  holds from 0.6.0.
- **The release tarball is laid out as an install prefix.** It holds
  `bin/mkfs.ext4`, `share/rust-fs-ext4/CAVEATS` (from `packaging/CAVEATS`,
  the notes an installer shows) and `LICENSE`, so an installer copies it
  whole and never names a tool, and tools added later arrive with no
  installer change. `scripts/package-cli.sh` builds it and fails unless it
  holds exactly those files and the tool answers `--help` and `--version`;
  `tests/scripts/test-package-cli.sh` tests that script. The release job
  also attests each tarball's build provenance, checkable with
  `gh attestation verify <tarball> --repo christhomas/rust-fs-ext4
  --signer-workflow christhomas/rust-fs-ext4/.github/workflows/release.yml`.
- **`mkfs.ext4 --version` names the published crate**:
  `mkfs.ext4 (am-fs-ext4) <version>`, where it said `fs-ext4`, a name no
  package carries.

- **Four output budgets are re-measured, and raised to the ~30% headroom the
  table names as its own convention** (#422): `unit` 700/42,000 →
  850/50,000, `lwext4` 80/6,000 → 85/6,800, `suite` 2,500/125,000 →
  2,900/140,000 and `vm` 2,700/135,000 → 3,050/148,000, each from the
  retained tier logs of CI run 36400527061. The rows stood still while the
  tiers grew into them, so a tier could fail with exit 65 and nothing red
  inside it — as `unit` did on #370 at 751/45,748. `images`, `kernel`,
  `wasm`, `scripts` and `semver` are unchanged: no measurement asked them to
  move.
- **Documentation that had drifted from the code is corrected** (#307).
  `apply_fallocate_punch_hole` handles extent trees of any depth, not
  depth 0 only; `fs_ext4_mount_with_fs_core_device` and its lazy variant take
  a reference on the underlying device, not on the handle;
  `docs/format-conformance-gaps.md` is current status, with the part still
  open (inline directories' `system.data` spill) named and the timestamps
  the driver stamps itself after 2038 closed by #324; and the README says
  how to get the git hooks. The 0.4.1 section below gains the two
  allocator fixes it shipped without listing.

- **The minimum supported Rust is declared: 1.87** (`rust-version` in
  `Cargo.toml`), so an older toolchain gets cargo's "requires rustc 1.87"
  refusal instead of seven `E0658` errors about `is_multiple_of`. The
  library and binaries are built at exactly that version by
  `chore check:msrv` and by a CI job `ci-ok` requires (#333).
- Small internal cleanups (#333): a directory-block allocation is committed
  from its plan alone and a plan for more than one block is refused rather
  than marking one and applying the counters of all of them; the uninit
  flag bits come from `BgdFlags`; the descriptors the allocators plan
  against are cached once an uninit flag is cleared, instead of cloned on
  every allocation, and the cache is dropped whenever a transaction
  publishes a clear or the descriptors are re-read; the two `fs_core`
  mount entry points share one body.
- **Path functions go through the inode-addressed core, and refuse what
  it refuses** (#372). `apply_rmdir` of a path ending in `.` is
  `InvalidArgument` (and `..` is `DirectoryNotEmpty`), and `apply_rename`
  with `.` or `..` as either final component is `InvalidArgument`: each
  used to remove or re-file the directory's own entry. The subtree check in `apply_rename` walks `..` instead of
  comparing path prefixes. A symlink target holding a NUL byte is refused.
  A path that resolves to a freed or reserved inode fails with
  `Error::Stale`.
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

- **An extent merge stops at the extent length limit.** Contiguous extents
  were merged with no cap on the sum. An initialized extent longer than 32768
  blocks encodes as an uninitialized one of `len - 32768`, so a file grown
  past 128 MiB in physically contiguous pieces read back as zeros and leaked
  its tail blocks; two uninitialized extents past 32767 overflowed `ee_len`.
  Every merge now refuses a pair over 32768 initialized or 32767
  uninitialized blocks, as the kernel does (#387).
- **fsck counts an uninit group the way the format defines it** (#391). The
  free-count audit counted zero bits in every group's on-disk bitmaps, but a
  `BLOCK_UNINIT` or `INODE_UNINIT` group's bitmap block is unspecified —
  `mke2fs` never writes it. A clean multi-group volume could then report
  `BlockGroupFreeCountDrift`/`SuperblockFreeCountDrift`, and repair wrote the
  raw counts into the descriptor and superblock, which `e2fsck` reports as
  "Free blocks count wrong". A `BLOCK_UNINIT` group's bitmap is now rebuilt
  from its own metadata, through the helper the allocator uses, and an
  `INODE_UNINIT` group counts every inode free.

- **A lazy mount refuses writes until its dirty journal is replayed**
  (#375), as `mount_lazy` always said it did. Nothing enforced it: the first
  write committed at the head of the log, over the transactions the last
  writer committed but did not checkpoint, and then marked the journal clean,
  so they were lost and the write itself was planned against pre-replay
  metadata. A write now fails with the new `Error::JournalNotReplayed`
  (`EROFS` through the C API) until `replay_journal_if_dirty` has put the log
  on the device. The same holds for a mount that was read-only when it
  mounted, whose replay went into the cache alone: its device turning
  writable does not make the on-disk log any less dirty.
- **A pwrite that fails part-way leaves the extent tree as it was.** A write
  into a file whose extent tree is deeper than the inode wrote every tree
  node it rewrote straight to the device, ahead of its transaction, so the
  next insert in the same call could read it back. When the call then failed
  — out of space on a later sub-run, a failed commit, a power cut — the
  on-disk tree mapped blocks the bitmap still called free, and the next
  allocation cross-linked them; on a journaled volume the write also went
  around the journal. The nodes are now staged in the transaction only, and
  the call's planner and lookups read them back from there (#389).
- **`set_flags` changes only the bits a caller may change** (#381). It
  refused only `EXTENTS`, `INLINE_DATA` and `EA_INODE`, so `INDEX_FL` on a
  linear directory made the next create or lookup read its first block as a
  `dx_root`, `HUGE_FILE_FL` changed the unit of `i_blocks`, and `ENCRYPT` or
  `VERITY` made a file unreadable. A change to any bit outside
  `inode::USER_MODIFIABLE_FLAGS` — the kernel's `EXT4_FL_USER_MODIFIABLE`
  less `EXTENTS`, `DAX` and `CASEFOLD` — now fails with `InvalidArgument`
  and writes nothing; a bit already set may be passed back unchanged. The
  header's `EXT4_NOATIME_FL` value is corrected to `0x80`.
- **A group descriptor counter that crosses 65536 carries into its own high
  half** (#390). The counter patch wrote the high halves of free blocks, free
  inodes and used directories at 0x2A/0x2C/0x2E, while the format (and the
  parser) has them at 0x2C/0x2E/0x30. A carry or borrow across 65536 then
  wrote into the top of the inode table pointer or into the neighbouring
  counter. It bites groups with more than 65535 free blocks or inodes, which
  16 KiB and larger blocks allow. The offsets are now named constants the
  parser and both writers share.
- **The kernel finds every attribute in an external xattr block this crate
  wrote (#379).** The block's entries were sorted by namespace and name, but
  the kernel's lookup compares namespace, then name *length*, then name, and
  stops at the first entry past the one it wants. With `user.abc` written
  before `user.zz`, the kernel's `getxattr("user.zz")` answered ENODATA on a
  volume `e2fsck` called clean, and every edit re-sorted a kernel-written
  block the same wrong way. Entries are now written in the kernel's order.

- **Orphan recovery reads no block map out of a fast symlink or an inline
  file** (#384). It freed a legacy block map whenever `i_blocks` was
  non-zero, but `i_blocks` also counts an external xattr block, so a fast
  symlink or inline file holding one — a security label that does not fit
  in the inode — had its target text or data read as direct pointers, and
  the blocks they named, other files' blocks, were freed. Text naming no
  block left the member on the chain, and every orphan behind it with it.
  Recovery now decides as unlink does (`holds_block_map`: not inline, and
  more sectors than the xattr block), and the xattr block is still
  released.
- **A preallocation that needs a fifth extent deepens the tree** (#423).
  `fallocate(KEEP_SIZE)` inserted only into the inode's inline extent root:
  on a file that already held four extents it stopped with
  `LEAF_FULL_NEEDS_PROMOTION`, and on a file whose tree was already deeper
  it was refused. It now promotes a full root and descends a deep one, as
  `pwrite` does, drawing the node blocks in the same transaction and
  counting them in `i_blocks` and the free-block counts.

- **A rename within an indexed directory whose new name splits a full leaf
  no longer fails with `NotFound` (#392).** The new name is added first, and
  when its leaf is full the leaf splits, moving its upper-hash half to a new
  block past the directory's old end. The old name was then removed through
  the directory's inode as read before the split, whose size stops short of
  that block, so an old name among those moved was not found and the rename
  was refused. Both rename paths now re-read the source directory through
  the transaction's buffer before removing from it. Nothing was written, so
  no volume was damaged. `tests/htree_dir_writes_oracle.rs` renames such a
  name in an index built by `e2fsck -D` and has `e2fsck -fn` judge it.
- **An extended attribute whose value moves between the inode and the
  external block keeps one copy (#377).** Growing an in-inode value past the
  inode's capacity wrote the new value to the block and left the old one in
  the inode, and the reader, which returns the first copy it finds, went on
  answering with the old value; `listxattr` listed the name twice. Shrinking
  a block value back into the inode left the block's copy, which came back
  as the value once the attribute was removed. `setxattr` now removes the
  other copy in the same transaction, and `removexattr` removes it from
  both places.
- **The external xattr block is checked before it is edited or read
  (#378).** Every writer read the block `i_file_acl` names and went ahead:
  a block without the xattr magic was formatted as an empty xattr block
  over whatever it held -- another file's data, a directory block -- and
  the call returned success; a block whose checksum failed was edited and
  restamped, blessing the corruption, and an unverified `h_refcount` was
  decremented on unlink. `setxattr`, `removexattr`, the release an unlink
  or truncate does, and `getxattr` / `listxattr` now refuse a block without
  the magic or with `h_blocks` other than 1 (`Corrupt`) and one that fails
  its checksum (`BadChecksum`), as the kernel does.
- **An xattr set on an inode with `i_extra_isize = 0` goes where the kernel
  reads it (#380).** Such an inode -- left by `ext2.ko`, older kernels, or
  a 256-byte-inode ext2 volume -- has no in-inode xattr area as far as the
  kernel and libext2fs are concerned: they read 0x80.. as the extra fields.
  `setxattr` put the area at 0x80 anyway, invisible to the kernel and over
  its timestamp bits, and on `metadata_csum` then stored a checksum high
  half of 0 over the area's magic. `setxattr` now gives the inode
  `i_extra_isize = 32` (zeroed, as the kernel does) before writing the area, and the readers and `removexattr` no longer
  treat bytes at 0x80 as an area. Every inode-checksum writer now stores
  `i_checksum_hi` only where `i_extra_isize` covers it, and verification
  compares only the low 16 bits where it does not, as the kernel does.

- **A lazy mount's journal replay brings the mount up to date with it**
  (#376). `replay_journal_if_dirty` replayed onto the device and left the
  mount planning against the superblock and descriptors it read before the
  replay, and committing through a journal writer opened over the dirty log.
  The writer's next commit wrote back its pre-replay sequence, below the
  replayed tail, so a later replay could walk from it into an older
  transaction; and a group whose `INODE_UNINIT` the replay cleared was still
  flagged in the mount's copy, so the next create in it was handed the inode
  the replayed transaction had allocated. The replay now reloads both and
  reopens the writer from the replayed journal superblock, as an eager mount
  does. A lazy mount that was read-only when it mounted is given its journal
  writer by the same call, where its writes used to go to the device
  unjournaled.
- **An unaligned punch-hole or zero-range changes only its bytes.** The punch
  rounded its byte range out to whole blocks and freed them, so
  `punch(100, 100)` on a file zeroed bytes 0..4096; zero-range, a punch plus
  a preallocation, did the same. The result was a consistent volume holding
  the wrong data, which `e2fsck` cannot see. Only the blocks wholly inside the
  range are freed now, and the covered part of a block at either edge is
  zeroed in place in the same transaction, as the kernel does (#388).
- **A mutation of an inline-data directory is refused instead of writing
  through its bytes** (#382). No directory writer checked `INLINE_DATA_FL`,
  and `map_inode_logical` read every non-extent inode's `i_block` as a block
  map, so an inline directory's parent inode number became its block 0.
  Renaming one across parents wrote the new parent's number into bytes
  12..16 of whatever block the old parent's inode number named — a bitmap,
  an inode table — and returned `Ok`; creates, links and removals inside
  one parsed that foreign block as entries. `map_inode_logical` now refuses
  an inline-data inode with `Unsupported`, so every create, mkdir, symlink,
  link, unlink, rename and rmdir whose parent or target is an inline
  directory fails before it writes. Renaming one within a block directory
  still works, and reading inline directories is unchanged.
- **A content write to an inline-data file is refused instead of freeing
  its bytes as blocks** (#383). `apply_replace_file_content` sent every
  non-extent inode to the block-map path, which read an inline file's first
  bytes as direct pointers and freed the blocks they named — content
  `"1\n"` names block 2609 — leaving `INLINE_DATA_FL` set over block
  pointers the reader then returned as the file. `apply_truncate_grow`
  patched only `i_size`, past what the inline area holds, which the reader
  rejects as corrupt. Replace, pwrite, and truncate in both directions now
  fail with `Unsupported` on an inline-data file and leave it whole.
- **Link counts stop at 65,000, and a `DIR_NLINK` count of 1
  stays 1 (#385).** A count was moved by plain arithmetic into a `u16`: the
  65536th hard link wrapped the count to 0, a name on an inode the next
  orphan pass or e2fsck treats as deleted, and nothing enforced the kernel's
  65000. On a `dir_nlink` volume a directory past 65000 links is written as
  1, "too many to count"; an rmdir under one wrote 0, which Linux refuses to
  load, and a mkdir wrote 2, which e2fsck reports. Counts now move as the
  format documentation's `i_links_count` entry says: a link, mkdir or
  directory rename that has no room returns the new `Error::TooManyLinks`
  (`EMLINK`) and writes nothing, a directory past the maximum is pinned at 1
  where `DIR_NLINK` allows it, and a directory count of 1 or 2 is never
  decremented. fsck no longer reports, or "repairs", the pinned 1.
  `tests/dir_nlink_oracle.rs` has e2fsck judge a mkdir and rmdirs under a
  65001-subdirectory parent the Linux kernel built.
- **A renamed FIFO, socket or device node keeps its entry's file type
  (#386).** `apply_rename` mapped the inode's mode to the directory entry's
  type byte knowing only regular files, directories and symlinks, and filed
  everything else under type 0, so on a `filetype` volume the entry
  disagreed with its inode: e2fsck pass 2 reported it and `d_type` readers
  saw `DT_UNKNOWN`. Link, rename, mknod and fsck now share one mapping,
  `DirEntryType::from_mode`, and `tests/rename_file_type_oracle.rs` has
  e2fsck judge one of each kind renamed.
- **`mkfs` at 16 KiB blocks and larger makes volumes e2fsprogs can open**
  (#429). Blocks per group was `8 * block_size`, 131072 and up from 16 KiB
  blocks, past the format's 65528-block group; `e2fsck`, `dumpe2fs` and
  `debugfs` refused the volume as a corrupt superblock. The group is now
  capped at 65528 blocks (`mkfs::MAX_BLOCKS_PER_GROUP`), what `mke2fs -b`
  chooses, and the bitmap bits past it are padded as mke2fs pads them.

- **Inline-data directories are read.** An inline directory's `i_block`
  holds its parent's inode number in bytes 0..4 and its entries from byte 4,
  continuing in the `system.data` xattr once they outgrow it. Lookup parsed
  from byte 0, so every name in a kernel-made inline directory failed with
  `bad rec_len`; readdir refused the directory as a legacy one; `fsck` skipped
  it. All three now read `.` and `..` synthesised as the kernel does, the
  entries from byte 4, and the continuation, checked against directories the
  kernel made (#427).
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
- **fsck verifies directory-block checksums and never restamps one it did
  not verify (#344).** The audit had no directory checksum check, and the
  `..`, bogus-entry and duplicate-dirent repairs recomputed a block's
  checksum without checking the old one, so a repair laundered a corrupt
  block with a fresh checksum and the damage was never reported. The audit
  now reports `Anomaly::DirBlockChecksumMismatch` (C ABI kind
  `dir_block_checksum`) with the directory inode and logical block, for a
  linear block's dirent tail and an htree index's `dx_tail`, as e2fsck's
  pass 2 does. The repair pass fixes a linear block's checksum when the
  block passes the structural checks, leaves an htree index reported, and
  every other repair refuses to restamp a block whose checksum still
  fails. A `..` repair in an htree root now restamps the root's `dx_tail`
  rather than a dirent tail. Repairs run in e2fsck's order, with link
  counts re-counted after the dirent edits, so fixing a wrong `..` no
  longer "repairs" two link counts to match the mistake. The audit also
  no longer loops forever on a malformed record in a directory's first
  block. Adding the variant is a breaking change for an exhaustive
  `match` on `Anomaly`.
- **Five tests that could pass over a broken implementation now fail on
  one** (#331). The journal's block-offset overflow branch is called
  directly, since every image-level case was refused by the bounds check
  first. The 32 MiB `gdt_csum` write is a block-indexed pattern read back
  whole through `debugfs`, so a data block written to the wrong place or
  not at all is seen. The htree hash table carries the empty name for
  every version and both seeds, taken from `debugfs dx_hash`. The library
  tests no longer swap the process-wide panic hook, which could swallow a
  parallel test's failure message, and a script guard keeps `set_hook` out
  of `src/`. `repro_wants_dir_symlinks` deletes its images unless
  `RFE_KEEP_IMAGES` is set, and a script test fails a run that leaves
  images in `tmp/`.

- **A revoke block's record count is bounded by the block, not clamped to
  it.** Replay read records up to `min(r_count, block length)`, so on a
  CSUM_V2/V3 journal the four-byte `r_checksum` tail was read as a record,
  and without checksums a count past the block was silently cut short. A
  checksum that happened to match a logged block revoked a committed write.
  A count past the block, less its tail when the journal has checksums, is
  now `Corrupt`, as the kernel and `e2fsck` refuse it (#301).
- **Moving a directory to another parent refuses a corrupt directory block
  instead of re-stamping it.** On a `metadata_csum` volume the rename
  rewrote `..` in the moved directory's first block and recomputed its tail
  checksum without verifying the old one, so a bad checksum was replaced by
  a valid one over the corrupted contents. Nothing earlier in the rename
  reads that block. It is now verified before the edit, as the indexed
  branch already was, and a mismatch is `BadChecksum` with the block left
  untouched (#322).
- **A direct commit that fails part-way no longer cross-links blocks.**
  Without a journal, a commit wrote its blocks in block-number order, so a
  group descriptor clearing `BLOCK_UNINIT` reached the disk before the
  bitmap it vouches for; and a write error returned without marking the
  mount, which stayed writable while still believing the group uninit.
  When only the final superblock write failed, the next allocation handed
  out the block just taken and `e2fsck -fn` reported it multiply-claimed.
  Bitmaps and every other block now go first, then the descriptors that
  clear an uninit flag, then the superblock, each stage flushed; any
  failure leaves the mount refusing writes with `ReadOnly`, its `flush`
  failing, and the volume marked not clean (#319).
- **A group descriptor cannot point its bitmaps or inode table at the
  superblock.** `bg_block_bitmap`, `bg_inode_bitmap` and `bg_inode_table`
  were bounded only by the end of the filesystem, so a pointer of 0 -- or 1
  on a 1 KiB volume -- was accepted at mount, and the next create wrote a
  bitmap block over the primary superblock or the descriptor table. Mount now
  refuses, as the kernel does at mount, any pointer inside
  group 0's superblock, descriptor table or reserved growth, and, without
  `flex_bg`, any pointer outside the descriptor's own group, with
  `Corrupt` (#320).
- **Dropping a full htree index is in the same transaction too (#347).**
  Where a leaf could not split because its parent index block was full,
  the index was dropped by committing what the operation had staged and
  then rewriting the directory's inode and index blocks straight to the
  device. A crash inside the drop left a half-converted index, or a new
  inode or link count the directory never gained a name for. The drop is
  now staged into the operation's `BlockBuffer`, so a create, link, mkdir
  or rename that forces it commits once.
  `tests/htree_split_write_cut.rs` cuts such a create after each write and
  requires `e2fsck -fn` to accept every remounted image; 22 of 42 cuts
  were rejected before.
- **`write_inode_raw` is refused where every other write is.** It is
  public and wrote straight to the device after checking only the
  length, so an outside caller wrote an inode onto a volume carrying a
  feature this driver must not write (`QUOTA`, `ORPHAN_PRESENT`, an
  unknown `RO_COMPAT` bit, …) and left the volume marked clean after
  modifying it. It now goes through the same refusal, and its first
  write marks the volume not clean. A read-only device answers
  `ReadOnly` rather than the device's `Corrupt`. Every public writer is
  now covered by one test that proves the refusal writes nothing (#323).
- **A group is bounded by its bitmap, and a filesystem by its device.**
  Nothing tied `s_blocks_per_group` to the bits in one bitmap block, or
  `s_blocks_count` to the device, so a group larger than its bitmap let
  `find_free_run` return a run past the bitmap's end — blocks in the next
  group, possibly in use — and one group of `u32::MAX` blocks on a
  megabytes-sized image let `read_all` ask for terabytes or a directory
  scan spin for 2^32 blocks. Mount now refuses `blocks_per_group` over
  `8 * block_size` (under bigalloc, `clusters_per_group`, with
  `blocks_per_group` exactly that many clusters) and a filesystem larger
  than its device, as the kernel does; `find_free_run` clamps to the
  bitmap with checked arithmetic; and the whole-file and directory bounds
  are checked and device-bounded rather than saturated (#321).
- **Automatic timestamps past 2038 read back as themselves (#324).**
  Every time the driver stamps on its own — the four times of a created
  file, directory, symlink or device node; mtime and ctime on a write,
  truncate or fallocate; ctime on chmod, chown, set-flags, an xattr change
  and utimens — wrote the 32-bit base only, with the epoch bits left zero
  (ctime's `*_extra` was explicitly zeroed). From 2038-01-19 each such
  time read back as 1901. They now go through the same encoding
  `utimens` uses, and on a 128-byte inode, which has no `*_extra` fields,
  a time past 2038 is clamped to 2038-01-19 03:14:07 as the kernel does,
  rather than wrapped. `i_dtime` stays the kernel's unsigned 32 bits.
  Checked by `debugfs stat` in the harness VM
  (`tests/timestamps_past_2038_oracle.rs`).
- **fsck reports every Directory-typed dirent that names a non-directory.**
  The audit marked an inode visited before checking its type, so when two
  such dirents named one regular file only the first was a `BogusEntry`;
  repair fixed that one and the rescan reported the other, so one pass did
  not converge and `initial - repaired != remaining`. An inode is now
  marked visited only once it is known to be a directory (#325).

- **The fuzz workflow no longer runs its dispatch input as shell, and
  keeps its reproducers when it is cancelled (#304).** `fuzz.yml` pasted
  the `seconds` input into its `run:` line, so whoever could dispatch it
  could run commands; it now arrives through `env:`, and
  `scripts/fuzz-all.sh` refuses anything but a positive whole number of
  seconds whose total across the targets fits the job's timeout, before
  anything is fuzzed. The reproducer upload runs on `failure() ||
  cancelled()`. A target with no seed corpus, or a fuzzer that exited
  non-zero without leaving a reproducer, is now reported as what it is
  rather than as a crash.
- **A read-write mount no longer holds every block it has written.** Each
  journaled commit pinned its blocks in the buffer cache, where nothing
  released them until unmount, so a long mount doing a bulk copy or a large
  repair grew without bound whatever the cache capacity. The journal writer
  checkpoints before `commit` returns, so the pin bought nothing: the
  commit's write-through now leaves ordinary clean entries. A read-only
  mount's replayed journal blocks, which are not on the device, stay pinned.
  `Filesystem::cache_pinned_blocks` reports the pinned count (#328).
- **The fuzz corpus seeds the htree decoder.** `mke2fs -d` writes every
  directory as a linear list, so no committed seed held a real `dx_root`
  and the explorer and the `fuzz_decoders` gate never started from one
  (#305). `scripts/make-fuzz-corpus.sh` now indexes the ext4 filesystems
  with `e2fsck -fyD`, takes the root directory's first block from the
  root inode's own block map, and asserts that the root has
  `EXT4_INDEX_FL` and that the block parses as a `dx_root`. The ext2 seed
  stays linear on purpose. A rebuild replaces only the files the script
  wrote, so committed reproducers survive it.
- **The release workflow can no longer half-publish a release, run a
  moved tag with a write token, or read a crates.io outage as "not
  published"** (#329). The `mkfs.ext4` matrix legs upload artifacts, and
  one `release` job — the only job with `contents: write`, running no
  third-party action — publishes every tarball once all legs passed.
  Every action in `release.yml` is pinned to a commit SHA, and every
  checkout sets `persist-credentials: false`. The crates.io check,
  now `scripts/crates-io-published.sh`, reads the HTTP status: 200 skips
  the publish, 404 publishes, anything else is retried and then fails.
  `tests/scripts/test-write-jobs-pinned.sh`,
  `tests/scripts/test-release-outside-matrix.sh` and
  `tests/scripts/test-crates-io-check.sh` hold each of the three.
- **`utimens` refuses a nanosecond count of one billion or more, and
  honours `UTIME_NOW` / `UTIME_OMIT`.** `apply_utimens` and
  `fs_ext4_utimens` stored 1e9..2^30-1 verbatim, which reads back as an
  impossible `tv_nsec`, and masked anything larger into an unrelated
  value. Such a count is now `InvalidArgument` (EINVAL), refused before
  the inode is touched. The two values `utimensat(2)` defines in that
  range are supported per field with Linux's numbering: `UTIME_NOW`
  (`FS_EXT4_UTIME_NOW`) takes the mount's clock and `UTIME_OMIT`
  (`FS_EXT4_UTIME_OMIT`) leaves the field alone, the seconds beside
  either being ignored; omitting both writes nothing (#326).
- **Four unchecked sizes no longer panic or overwrite a neighbouring
  block (#327).** The deep extent-insert descent sliced index entries
  without a bounds check, so a root or child node claiming more entries
  than it holds panicked; it now returns `CorruptExtentTree`. A journal
  commit refuses, before any I/O, a transaction whose block size or any
  write's length is not the volume's block — `writes` is public, so a
  2x-block entry bypassed `add_write` and overwrote the next block, and a
  short superblock entry panicked. The group-descriptor table's length is
  converted with `try_from`, so 2^32 groups on a 32-bit target are refused
  rather than truncated to an empty table. And the reserved-inode floor
  reaches past group 0: with `s_first_ino > inodes_per_group + 1`, group 1
  no longer hands out reserved inodes.

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
  the first index entry, and the leaf below it reports the hole (#260).
- **An insert in front of a leaf corrects the keys above it.** Every index
  entry holds the first logical block of the child it names, and an extent
  inserted before a leaf's first entry moves it. The keys were left as they
  were, so `e2fsck` reported `Logical start N does not match logical start M
  at next level` — reachable as soon as a leading hole could be written at
  all. `plan_insert_extent_deep` now carries the correction up the path
  beside any split it is propagating (#260).
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
- A rename onto itself validates and resolves the path before succeeding.
  `apply_rename(p, p)` returned `Ok` before looking at `p`, so a NUL name
  renamed onto itself got past the #247 refusal and a missing path onto
  itself succeeded where rename(2) gives ENOENT; both now fail, an existing
  path onto itself still succeeds without a write, and none of the three
  marks the volume not clean (#303).

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

- **A group's uninitialised flags are cleared when it is first allocated
  from** (#41) — allocating from a group set its bitmap bits but left
  `INODE_UNINIT` / `BLOCK_UNINIT` standing, which licenses every reader, the
  next mount included, to treat the whole group as free. On a multi-group
  volume the next mount handed out the same inode, or the same block, again.

- **A rebuilt block bitmap reserves what it must not hand out** (#44) — the
  bitmap rebuilt the first time a group is allocated from now places backup
  superblocks by the filesystem's own layout (every group without
  `SPARSE_SUPER`, the two `s_backup_bgs` groups with `SPARSE_SUPER2`) rather
  than the classic sparse rule, reserves `s_reserved_gdt_blocks`, and makes a
  cleared `BLOCK_UNINIT` visible to later allocations only once the commit
  that clears it has succeeded.

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
