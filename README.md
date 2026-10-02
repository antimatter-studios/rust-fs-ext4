# am-fs-ext4 — pure-Rust ext2/3/4 driver

Pure-Rust read/write driver for the ext2, ext3, and ext4 on-disk
formats. Mounts disk images and block devices, journals writes
through JBD2, replays the journal on dirty mounts, and exposes a
stable C ABI (`fs_ext4_*`) so any FFI host (Swift/C/C++/Go/…) can
link `libfs_ext4.a` and `#include "fs_ext4.h"`. MIT-licensed. Zero
kernel calls; zero non-MIT/BSD/Apache dependencies. Runtime crates
are `crc32c`, `bitflags`, `unicode-normalization` and `caseless`
(casefold), plus the sister `am-fs-core` block-device crate.

Designed for FFI: the C ABI is stable and the static library has no
host-specific assumptions, so the same `libfs_ext4.a` works equally
well in a macOS FSKit extension, a Linux FUSE binary, a Windows tool,
or any embedded environment with a `BlockDevice` shim.

## Status

Read/write driver for the common case across ext2, ext3, and ext4.
Mount + read is exhaustive against the ext4 feature matrix we test
against; write is journaled through JBD2 and crash-safe under
fault-injection sweeps for every multi-block op the driver
exposes. Specific gaps are listed under "What doesn't work" below.

- 700+ automated tests — 200+ lib unit tests and 450+ integration
  tests across ~100 test binaries (`cargo test --release`).
- All 15 multi-block write ops (`5.2.1`–`5.2.15` of the write-support
  plan) committed atomically through the JBD2 writer with explicit
  crash-safety sweeps.
- ext2 and ext3 RW are unlocked (Phase 9.1/9.2/Phase B) via the
  same flavor-aware extent/indirect dispatcher.

## Features

### Read

- Mount ext2 / ext3 / ext4 images and block devices (block sizes
  1 KiB, 2 KiB, 4 KiB).
- Inode parse with extra-fields support; `stat`, `readdir`,
  `readlink`, `read`, `getxattr`, `listxattr`.
- Extent trees (depth 0, depth 1, depth ≥ 2 read; uninitialized
  extents present as zero on read).
- Indirect-block trees (legacy ext2/3 — direct, indirect,
  double-indirect, triple-indirect).
- HTree directory traversal (legacy / `half_md4` / `tea` hash).
- Inline data (≤ 60 bytes inline file body, plus inline xattr
  overflow).
- External xattr blocks; ACL attributes (`system.posix_acl_*`).
- `metadata_csum` and `csum_seed` verification on every block that
  has an on-disk checksum (superblock, BGD, inode table, dir tail,
  extent tail, htree tail, xattr block, journal commit/descriptor
  blocks).
- JBD2 journal replay on mount: descriptor / commit / revoke
  blocks all honoured, including v2 (csum-tail) format.
- Read-only audit pass (`Filesystem::audit`) reconciles link
  counts and reports dangling directory entries.
- Optional LRU block cache (`CachingDevice`) for callback-mode
  hosts where every read hits a remote/RPC boundary.

### Write

All mutating ops route through the same multi-block transaction
buffer and commit atomically through `JournalWriter`:

- `chmod`, `chown`, `utimens`, in-inode `setxattr` / `removexattr`.
- `create`, `unlink`, `mkdir`, `rmdir`.
- `link` (in-place), `symlink` (inline + slow path), `rename`
  (POSIX semantics: `EEXIST` on no-clobber, `EINVAL` into own
  subtree, cross-parent `..` updates).
- `truncate` shrink and grow (sparse-grow leaves holes; reads
  return zeros).
- `setxattr` / `removexattr` on the external xattr block (alloc
  on first overflow, free when the block becomes empty).
- File replace (full-content overwrite via fresh extent allocation).
- Inline-data files: pwrite, replace and truncate in the inode while the
  result fits, converting the file to extents in the same transaction
  when it does not.
- Inline-data directories: entries added and removed in the inode while
  they fit, converting the directory to a block in the same transaction
  when they do not.
- `fallocate(KEEP_SIZE)`, `fallocate(PUNCH_HOLE)`,
  `fallocate(ZERO_RANGE)`.
- Extent-tree mutation: depth 0 → 1 promotion and depth-1 inserts.

Crash-safety guarantees: the four-fence JBD2 protocol
(journal-write → dirty-flag → final-write → clean-flag) is enforced
per transaction with an explicit flush between each fence. A
post-remount state is always either pre-op or post-op — never
torn. Pinned by parameterized fault-injection sweeps (see "Test
contract" below).

### Filesystem variants

| Variant | Read | Write |
|---|---|---|
| ext2 | done | done (indirect-block extents, no journal) |
| ext3 | done | done (indirect-block extents, JBD2) |
| ext4 | done | done (extent trees, JBD2) |

ext3 RW landed via Phase B's flavor-aware journal dispatch
(`jbd2::journal_block_to_physical` and `JournalWriter::open`
both branch on `EXTENTS_FL`). ext2 has no journal but otherwise
shares the same write helpers. `tests/mkfs_ext3_oracle.rs` formats both
flavors, writes through the crate and requires `e2fsck -fn` to pass. An
ext2/ext3 directory grows through its direct and single-indirect blocks
only; one that would need the double-indirect block is refused.

## Command-line tools

Work on an ext4 image or device directly, with no mount, kernel driver or
VM: an escape hatch for an errand (get a file out, put one in, read the
label, check whether it is dirty), not a place to do real filesystem work.
For that, mount it.

One multi-call binary, `rust-fs-ext4`, behind the `cli` feature so the
library gains no dependency from it:

```sh
cargo install am-fs-ext4 --features cli   # or: chore cli:install from a checkout
```

It dispatches on the name it is run as, so an install links each tool to
it; `rust-fs-ext4 <tool> ...` is the same program under the one name
nothing else on PATH can shadow. `cargo install` puts only `rust-fs-ext4`
on PATH and makes no links, so after it either run the tools through it
(`rust-fs-ext4 fs ...`) or link the names yourself:

```sh
cd "$(dirname "$(command -v rust-fs-ext4)")"
for name in $(rust-fs-ext4 generate names); do ln -s rust-fs-ext4 "$name"; done
```

`chore cli:install` makes those links for you.

```sh
mkfs.ext4 --size 64M --label BACKUP disk.img
fsck.ext4 disk.img                         # -y to repair
fs.ext4 disk.img ls /
fs.ext4 disk.img read /etc/fstab > fstab
fs.ext4 disk.img write /notes.txt < notes.txt
fs.ext4 disk.img mkdir /backup
fs.ext4 disk.img get label --text
fs.ext4 disk.img set label ARCHIVE
fs.ext4 --offset 1048576 whole-disk.img info
tar cf - ./dir | fs.ext4 disk.img write /dir.tar
rust-fs-ext4 doctor                        # is every tool on PATH ours?
```

- **Output.** Metadata is JSON on stdout by default (`--text` for people);
  `read` and `write` carry raw bytes. A failure is
  `{"error": "...", "code": N}` on stderr, and `N` is the exit status: 1
  failed, 2 wrong command line, 3 not implemented. `fsck.ext4` keeps
  fsck(8)'s statuses instead: 0 clean, 1 corrected, 4 uncorrected, 8
  operational error, 16 usage.
- **`get`/`info`** report `fs`, `label`, `total_bytes`, `free_bytes`,
  `block_size` and `dirty`, with ext4's own fields under `ext4`.
- **`set label <value>`** rewrites the label, at most 16 bytes, in the
  primary superblock and every backup; an empty value clears it.
- **Not yet:** `resize` answers `not implemented`, and no resize is planned.
  `fsck.ext4` checks this crate's audit, a subset of e2fsck's; a directory
  it cannot read is a finding it cannot repair (#445).
- **`--version`** on every name prints `<tool> (am-fs-ext4) <version>`.
  `rust-fs-ext4 doctor` resolves each name on PATH, checks it answers that
  way, and says what wins and the fix when it does not.
- `chore test:cli` tests the tools as installed (`doctor` first), and CI
  runs it on every pull request.

## What works

Per-operation, on a clean image:

- `mount` / `mount_rw` / `mount_with_callbacks` /
  `mount_rw_with_callbacks` / `mount_rw_with_callbacks_lazy`
  (lazy variant defers journal replay).
- `stat`, `readdir`, `readlink`, `read`.
- `getxattr`, `listxattr`, `setxattr` (in-inode + external block),
  `removexattr` (in-inode + external block).
- `chmod`, `chown`, `utimens`.
- `create`, `unlink`, `mkdir`, `rmdir`, `link`, `symlink`, `rename`.
- `truncate` (shrink and grow).
- `write` (replace-content; allocates fresh extents and frees old
  ones in one journaled transaction).
- `fallocate(KEEP_SIZE)`, `fallocate(PUNCH_HOLE)`,
  `fallocate(ZERO_RANGE)`.
- `audit` (read-only fsck — link counts, dangling entries).
- `fsck_run` (callback-mode audit exposed through the C ABI).
- `format_filesystem` (in-process `mkfs`; the C-ABI entry FFI hosts
  call to format a fresh volume is `fs_ext4_mkfs`).

## What doesn't work

Honest gap list — pulled from `docs/ext4-full-write-support.md`
and the in-tree TODOs:

- **Extent-tree depth ≥ 2 mutation.** Read works; write refuses
  with a structured error in `extent_mut.rs`. Phase 4 of the
  write-support plan covers the design (`docs/extent-tree-depth2-design.md`).
- **HTree internal split.** Leaf split is implemented; once a
  directory grows past the depth-1 leaf-block capacity (~340
  extents on 4 KiB blocks), further inserts return `ENOSPC`-shaped
  errors.
- **`extend_dir_and_add_entry` buffer-twin.** The fallback path
  used when an in-place dir-entry insert can't fit still does
  direct disk writes rather than going through `BlockBuffer`.
  Refactor pending (~150 LoC of `plan_promote_leaf` adaptation).
- **JBD2 journal modes.** The writer effectively runs `data=ordered`
  semantics today; `data=writeback` and `data=journal` are not
  selectable.
- **Orphan-list inserts on still-open unlink.** The driver
  doesn't see open-fd state today; the host (FSKit / FUSE) would
  need to plumb that through. Reads + replay of pre-existing
  orphan chains *is* implemented.
- **Casefold lookups.** The hash function is implemented in
  `casefold.rs`; HTree wiring is not.
- **fs-verity, fscrypt, project quota, user/group quota,
  online-resize, mmap shared writes** — not implemented.
- **EA refcount sharing on external xattr blocks** — single-owner
  only; never shares (deferred until a consumer needs it).
- **Indirect-path file replace is un-journaled.** The
  indirect-block replace helper writes data blocks directly. Only
  applies to ext2 mounts (which have no journal anyway) and
  ext3 mounts that disable the journal.

## Architecture

The driver is split into read modules, write modules, and the
JBD2 / transaction layer that ties them together.

- **`block_io`** — `BlockDevice` trait. File-backed and
  callback-backed implementations both live here; `CachingDevice`
  wraps either for LRU read caching.
- **`superblock`, `bgd`, `inode`, `features`** — on-disk parse +
  validate. All reject malformed geometry with structured errors;
  `cargo fuzz`-style harness in `tests/fuzz_smoke.rs` proves no
  panic on truncated / zeroed / bit-flipped inputs.
- **`extent.rs` vs `indirect.rs`** — the two ways an inode can
  map logical blocks. Read paths dispatch on `EXTENTS_FL`
  (`indirect::map_logical_any` / `extent::lookup`).
- **`extent_mut.rs`, `indirect_mut.rs`** — mutation planners.
  Compute "what the new tree would look like" without touching
  the device, then hand the planned writes to a `BlockBuffer`.
- **`BlockBuffer` (in `fs.rs`)** — accumulates bitmap, BGD, SB,
  inode-table, dir-block, xattr-block, and payload-data
  mutations for a single high-level op. `commit_block_buffer`
  routes the whole buffer through `JournalWriter` when a journal
  is present, falling back to direct writes when not.
- **`JournalWriter` (`journal_writer.rs`)** — owns the JBD2
  inode's extent map, allocates a fresh transaction slot,
  emits descriptor + data + commit blocks with the four-fence
  protocol, and rewrites the super-state to clean once the
  final-write phase completes.
- **`DeepReader`** — read-side helper that consults the
  in-flight transaction's pending writes before going to disk,
  so post-write reads inside the same transaction return the new
  data.
- **`verify.rs`** — structural verifier reconciling bitmap vs.
  inode-tree-claimed blocks; used both in tests and in the
  read-only audit FFI surface.
- **`capi.rs`** — C ABI. Thread-local error state with errno
  inference (`fs_ext4_last_error()`, `fs_ext4_last_errno()`);
  `catch_unwind` fence at every entry so a panic in the Rust
  interior never crosses the FFI boundary.

Deeper write-up of each phase lives in `docs/`:

- `docs/IMPROVEMENT-PLAN.md` — triaged stability / hardening backlog.
- `docs/ext4-full-write-support.md` — the 9-phase write-feature plan
  this README's roadmap mirrors.
- `docs/extent-tree-depth2-design.md` — the index-block split design
  for Phase 4.
- `docs/TEST-DISKS.md` — fixture / feature coverage matrix.

Spec sources: the on-disk format documentation on kernel.org's
ext4 wiki, plus Carrier, *File System Forensic Analysis*
(Addison-Wesley, 2005). Research-reference Rust/C code is
[`yuoo655/ext4_rs`](https://github.com/yuoo655/ext4_rs) (MIT) and
[`lwext4`](https://github.com/gkostka/lwext4) (BSD-2-Clause); both
credited in the License section.

## Test contract

- **Lib unit tests:** across most modules
  (`alloc`, `bgd`, `block_cache`, `block_io`, `casefold`,
  `checksum`, `dir`, `extent`, `extent_mut`, `features`, `fs`,
  `hash`, `htree`, `htree_mut`, `indirect`, `indirect_mut`,
  `inline_data`, `inode`, `jbd2`, `journal_apply`,
  `journal_writer`, `mkfs`, `path`, `superblock`, `transaction`,
  `verify`, `xattr`, …).
- **Integration tests:** ~100 test binaries in `tests/`,
  covering every C ABI entry point, end-to-end mutation
  round-trips, journaled-path regressions, htree large-dir
  growth, ACL parse, large-dir HTree, deep-extent reads,
  multi-tier indirect maps, ext2/3 RW round-trips, fuzz smoke,
  fsck audit, orphan recovery, sparse-grow, fallocate variants,
  and the cross-validators below.
- **Crash-safety sweeps** (parameterized over 0..=N "drop the
  next write" budgets, asserting post-remount state is always
  either pre-op or post-op):
  - `journal_writer_crash_safety.rs` — `chmod` (0..=20 budget).
  - `journal_writer_crash_dir_ops.rs` — `create` /
    `mkdir` / `link` / `symlink`.
  - `journal_writer_crash_rename_write.rs` — `rename`,
    file-replace, external xattr.
  - `journal_writer_truncate_shrink.rs` — `truncate_shrink`
    (0..=30 budget).
  - `journal_writer_unlink_rmdir.rs` — `unlink` / `rmdir`
    (0..=40 budget).
  - `fallocate_crash_safety.rs` — KEEP_SIZE / PUNCH_HOLE /
    ZERO_RANGE.
  - Orphan recovery's cut-after-every-write sweeps, on a real orphan
    through both the journaled and the unjournaled commit, are unit
    tests in `src/fs.rs` beside the helpers that plant one.
- **Tool oracles:** the driver writes and e2fsprogs reads it back —
  `e2fsck -fn` for consistency, `debugfs` for content and metadata
  (`tests/oracle_debugfs.rs`: dump + compare, `stat`, `ex`, `icheck`,
  `ncheck`, `logdump`, and a negative case proving a corrupted data byte is
  caught). Every one of those tools runs **inside the harness VM**, never
  on the host — see [Testing](#testing). `chore test:oracle` runs them all.
- **Kernel oracles:** the driver writes and **Linux itself** reads it back.
  `tests/kernel_readback.rs` (Rust API) and `tests/kernel_readback_capi.rs`
  (C ABI) build a tree — directories, a multi-megabyte file written in
  unaligned pieces, symlinks, xattrs, an ACL, a rename, an unlink, a
  truncate — then loop-mount the image read-only in the guest and compare
  names, modes, sizes, xattrs, ACLs and SHA-256 of every file against what
  was written. A third test goes the other way (the kernel writes, the C
  ABI reads), and a fourth flips one data byte and proves `e2fsck` still
  calls the volume clean while the readback catches it. `chore test:kernel`.
- **A third implementation:** `tests/lwext4_cross_validate.rs` compares
  this crate against [lwext4](https://github.com/gkostka/lwext4)
  (BSD-2-Clause, pure C), which shares a code lineage with neither the
  kernel nor us — so an ambiguity we and e2fsprogs read the same way
  still has somebody to disagree. It is built in the harness guest at a
  pinned commit by `scripts/vm-setup.sh`; `tests/lwext4/report.c` walks a
  volume through it. Compared in both directions — every fixture lwext4's
  feature set covers, a tree this crate wrote, and a tree lwext4 wrote —
  on names, permission bits, sizes, symlink targets and SHA-256 of
  contents. The fixtures it cannot read (`inline_data`, `large_dir`,
  `metadata_csum_seed`, a partition table) are asserted to be REFUSED
  rather than left out. `chore test:lwext4`.
- **Structural verifier:** `crate::verify::verify` reconciles
  the on-disk bitmap against every block claimed by the inode
  tree; pinned by `tests/verify_basic.rs`, also runnable as a
  post-mutation check from any integration test.

## Roadmap

Pulled from `docs/ext4-full-write-support.md` — items not yet
ticked. Numbering follows the plan doc.

- [ ] **3.5** EA refcount sharing on external xattr blocks.
- [ ] **4.1–4.6** Extent-tree depth ≥ 2 mutation (index-block
      split, recursive descent, merge-on-shrink, index csum).
- [ ] **5.3.1–5.3.3** JBD2 journal modes (`data=ordered`
      explicit / `data=writeback` / `data=journal`).
- [ ] **6.3** Orphan-list insert on still-open unlink (gated on
      host fd-tracking).
- [ ] **6.4** Link-count audit auto-repair under a recovery
      transaction.
- [ ] **7.2** Checked arithmetic in remaining hot sites (rare in
      practice; cosmetic).
- [ ] **7.3** Full FFI input-validation sweep.
- [ ] **7.4** Richer `Error` variants (cosmetic; `Corrupt(&str)`
      carries enough context today).
- [ ] **8.2** Cross-call extent-lookup memoization.
- [ ] **8.4** Coalesce adjacent dirty blocks inside the
      `BlockBuffer`.
- [ ] **9.3** Casefold HTree wiring.
- [ ] **9.4** Project quota.
- [ ] **9.5** User/group disk quota.
- [ ] **9.6** fs-verity.
- [ ] **9.7** fscrypt v2.
- [ ] **9.8** Online resize.
- [ ] **9.9** mmap shared writes (host-integration dependent).
- [ ] `extend_dir_and_add_entry` buffer-twin (the last direct-disk-write
      helper inside the otherwise-buffered write path).

## Changelog

Highlights from the last 50 commits, grouped by date.

### 2026-10-03 — 0.7.1 — the tools are released

- The release tarball ships every tool: `bin/rust-fs-ext4` with
  `mkfs.ext4`, `fsck.ext4` and `fs.ext4` linked to it, their man pages and
  the zsh, bash and fish completions, where 0.7.0 shipped only
  `mkfs.ext4` (#475).
- No library or C ABI change.

See [CHANGELOG.md](CHANGELOG.md#071--2026-10-03) for the full notes.

### 2026-09-30 — 0.7.0 — the volume label, whole and writable

- **Breaking (C ABI):** `fs_ext4_volume_info_t.volume_name` is 17 bytes, so
  a label that fills `s_volume_name`'s 16 is reported whole; every field
  after it moves, and C callers rebuild against the new header (#463).
- The volume label can be set after the volume is made:
  `Filesystem::set_volume_label` and `fs_ext4_set_volume_label` rewrite the
  primary superblock and every backup, as `tune2fs -L` does (#447).
- A formatted volume has `/lost+found`, laid out as `mke2fs` lays it out
  (#443).
- The fsck audit reports a directory it cannot read, and `fsck.ext4` exits
  4, where the volume used to audit clean (#445).

See [CHANGELOG.md](CHANGELOG.md#070--2026-09-30) for the full notes.

### 2026-09-30 — 0.6.0 — the provenance remediation, and the tools as one binary

- **Provenance.** `PROVENANCE.md` records where the code comes from and
  the 2026-09-29 audit. `src/hash.rs` is re-implemented clean-room; the
  inode timestamp helpers and five format-driven routines are restated and
  checked against e2fsprogs and a Linux kernel in the harness VM; and a CI
  check fails when the tree names kernel or e2fsprogs internals. Every
  earlier release on crates.io, 0.3.2 to 0.5.1, contains htree hash code
  derived from the Linux kernel; the claim that no code derives from GPL,
  LGPL or AGPL source holds from 0.6.0.
- **Breaking:** `XattrEntry` and `Error` are `#[non_exhaustive]` and gained
  fields and a variant, and `fsck::Anomaly` is `#[non_exhaustive]` too;
  `fs_ext4_readlink` returns the target's length;
  `Runtime::now_unix_seconds` returns `i64`;
  `casefold::casefold_name_hash` takes the directory's hash version and
  returns the hash a casefolded directory uses (#438). `chore check:semver`
  now refuses a public-API break the version does not declare.
- A major hash of `0xFFFFFFFE`, which the directory index reserves, is
  given as `0xFFFFFFFC`.
- The command-line tools are one multi-call binary, `rust-fs-ext4`, behind
  the `cli` feature: `mkfs.ext4`, an `fs.ext4` that lists, reads and writes
  an image without mounting it, and an `fsck.ext4` with fsck(8)'s exit
  statuses. The binary writes its own man pages and shell completions.
- Inline-data files and directories are written, not refused; the parsers
  are fuzzed; and the driver's writes are read back by a real Linux kernel.
- Dozens of format fixes: extent trees, htree splits, xattr placement,
  journal replay, group descriptor counters, link counts and timestamps
  past 2038. See [CHANGELOG.md](CHANGELOG.md#060--2026-09-30).

### 2026-09-06 — 0.5.1 — what the image says is checked before it is believed

- A journal entry that names a block outside the filesystem is refused
  rather than replayed. Replay writes where the entry says it should, so
  a crafted journal could write anywhere on the device.
- The superblock fields a mount sizes itself from are bounded, so an
  image cannot decide how much memory a mount spends.
- A directory's declared size no longer drives an unbounded scan.
- The allocator and the journal writer may not step outside the
  filesystem.
- A group descriptor's pointers are read as block numbers in this
  filesystem, which is what they are.

### 2026-09-04 — 0.5.0 — the formatter ships as a binary, and one type moves out

- **Breaking:** `CachingDevice` is no longer part of this crate's public
  API. It was removed while deleting what a dead-code `allow` was hiding;
  the type lives in `am-fs-core`, where the other drivers already got it.
  The functionality moved rather than disappearing, but
  `fs_ext4::block_io::CachingDevice` no longer resolves — which for a
  `0.x` crate is a minor bump, since cargo treats the minor as the
  compatibility boundary. Nothing outside this repo referenced it.
- `mkfs.ext4` is now published as a downloadable binary. A release job
  builds it, renames the cargo target `mkfs_ext4` to the dotted name the
  format's convention uses — cargo refuses a dot in a target name — and
  attaches a per-platform archive, so a package manager can install it
  without a Rust toolchain.
- A truncated inode read now fails closed. The checksum verifier had
  accepted a short read as a pass; a read that did not return the bytes
  is not a checksum that matched.
- Four format-conformance fixes: a bigalloc filesystem is refused rather
  than misread (its cluster arithmetic is not implemented, so every
  block-group offset would be wrong); timestamps apply the epoch-extension
  bits and are read as signed, so dates past 2038 and before 1970 are no
  longer 136 years out; a missing inline-data spill xattr is corruption
  rather than a silently truncated file; and a *writable* mount of an MMP
  filesystem is refused, read-only being unaffected.
- **The C attribute struct's timestamps widen to `int64_t`, moving every
  field after them.** Consumers must recompile, not relink.
- Dependencies move to `am-fs-core` 0.2.4.

### 2026-08-29 — 0.4.1 — the multi-group formatter in front of e2fsck

- `mkfs` output is now checked against `e2fsck` at three and five block
  groups, including a short final group, and both sizes run in CI. No bug
  was found: the arithmetic was right, it had simply never been checked.
- Three `mkfs.ext4` CLI bugs: `-c` consumed the device path and then
  failed about the path it had just eaten; `-b` reported a bad block size
  only after opening the device; `-q` suppressed warnings only when it
  came first.
- A rebuilt block bitmap reserves what it must not hand out, and a
  group's uninitialised flags are cleared when it is first allocated from.

### 2026-06-30 — 0.4.0 — multiple block groups + write-path/mkfs correctness

- Multiple block groups for ext4 (full GDT + per-group bitmaps/inode tables;
  sparse-super backups for `e2fsck -b` recovery).
- Correctness: on-write metadata checksums + SB free-count coherency;
  dir/xattr/pwrite write-path hardening; 1 KiB-block & ext3 mkfs fixes
  (validated by the new in-process + `e2fsck` oracle harnesses).
- (The deep-dir-extents / casefold / `fs_ext4_mknod` / `fs_ext4_set_flags` /
  extended-`fs_ext4_attr_t` features shipped in 0.3.0 — see `CHANGELOG.md`.)

### 2026-05-03 — Phase 1+3+5+6 rollup, ext2/3 RW, crash sweeps

- `64d6e6b` crash-safety sweeps for the remaining multi-block ops.
- `c800223` regression tests pinning rename / write / external-xattr
  to the journaled path.
- `a9fe115` Phase 5.2.5 / 5.2.10 / 5.2.15 — finish 5.2.x multi-block
  journal coverage.
- `7f51ece` Phase 5.2.9 + 5.2.14 — `unlink` + `rmdir` multi-block
  journaled.
- `455438d` Phase 5.2.8 / 5.2.11 / 5.2.12 / 5.2.13 — `create` / `link` /
  `symlink` / `mkdir` journaled.
- `165391c` Phase 5.2.6 `truncate_shrink` + multi-block tx
  (introduces `BlockBuffer`).
- `df526b5` Phase 1+3+5 write-path foundation — journaled writes,
  external xattr blocks.
- `5c4a2f8` Phase 2.3 + 2.4 — `fallocate` punch-hole + zero-range.
- `77a9ef3` Phase 2.2 — `fallocate(FALLOC_FL_KEEP_SIZE)`.
- `8edbd98` Phase 6.2 — orphan replay.
- `92fdcb9` Phase 6.1 — orphan list parser.
- `487eebd` Phase 6.2 crash-safety probe sweep.
- `47dfc33` ext3 RW unlocked via flavor-aware journal dispatch
  (Phase B).
- `76b36d8` write `s_journal_inum` at 0xE0 (not 0xD8) so ext3 images
  mount.
- `58558ed` Phase 8.1 + 8.3 — LRU block cache + vectorized bitmap
  scan.
- `8140470` Phase 7.5 write-path fuzz coverage + clippy fix in
  `verify.rs`.
- `5a0bfdf` Phase 1.2 + Phase 4 design doc.
- `1baa872` license sweep — scrub references to copyleft-licensed
  Linux/NTFS tooling.
- `9d2f2ff` `72bbad4` direct-qemu FreeBSD cross-validator scaffolding.

### 2026-05-01 — `mkfs_ext4` binary, lazy replay, fsck FFI

- `b788435` `54ec273` standalone `mkfs_ext4` binary
  (`--create-size` for one-shot create+format).
- `7f64a38` gate Unix-only `FileTypeExt` behind `cfg(unix)` for
  Windows builds.
- `987f8d5` `fs_ext4_mount_rw_with_callbacks_lazy` +
  `fs_ext4_replay_journal_if_dirty` for deferred journal replay.
- `2679e60` expose `fs_ext4_fsck_run` + `audit_with_callbacks`.
- `60974c2` CI validation of the binary's output.

### 2026-04-30 — Sandboxed RW callbacks

- `f08ec2e` `fs_ext4_mount_rw_with_callbacks` for sandboxed FSKit
  consumers.

### 2026-04-22 — 0.1.3, pre-commit hook

- `b182836` release 0.1.3.
- `4352d37` pre-commit hook (`cargo fmt --check` + `clippy`).

### 2026-04-20 — 0.1.1 / 0.1.2 releases

- `8248ff4` 0.1.2: expose `s_state` via
  `fs_ext4_volume_info_t.mounted_dirty`.
- `47dfbb5` 0.1.1: docs rewrite, neutral framing, plain-English
  disclaimer.

### 2026-04-19 — Stability hardening, perf, fsck audit

- `f8603e9` hard-cap extent tree depth; refuse cycles + spec
  overflow.
- `ed8223c` checked arithmetic on physical block offsets.
- `b7f805f` reject malformed geometry; never panic.
- `e1d2c82` `CachingDevice` — optional LRU read cache.
- `c684481` word-at-a-time scan in `find_first_free`.
- `c5ea3ba` memoize last extent across a single read call.
- `1e9bda4` read-only link-count audit (`Filesystem::audit`).
- `708dcff` fuzz-smoke harness (dir-walk, parser-fuzz,
  exhaustive bit-flip).
- `e1c18e6` IMPROVEMENT-PLAN.md — triaged gaps, phased roadmap.
- `d731e71` pin `rust-toolchain` to 1.94.1.

### 2026-04-18 — Crate rename, test-disk matrix

- `a8bcaa4` rename crate `ext4rs` → `fs-ext4`, C ABI
  `ext4rs_*` → `fs_ext4_*`.
- `2af0108` add `TEST-DISKS.md` — feature coverage matrix.

Full changelog in `CHANGELOG.md`.

## License

MIT — see [LICENSE](LICENSE). Copyright (c) 2026 Chris Thomas.

Research references credited under their own licenses:

- [`yuoo655/ext4_rs`](https://github.com/yuoo655/ext4_rs) — MIT.
- [`lwext4`](https://github.com/gkostka/lwext4) — BSD-2-Clause.
  Built in the harness guest at a pinned commit and used as a
  cross-validator (`tests/lwext4_cross_validate.rs`), never linked into
  the shipping binary.

Spec sources: the kernel.org ext4 and JBD2 format documentation; RFC
1320 and the TEA paper; Carrier, *File System Forensic Analysis*
(Addison-Wesley, 2005); BSD-licensed implementations for format facts;
and e2fsprogs and the Linux kernel used as black-box oracles. From 0.6.0
the driver contains no code derived from GPL, LGPL or AGPL source; see
[PROVENANCE.md](PROVENANCE.md) for the audit and for earlier releases.

## Building

Standard cargo:

```sh
cargo build --release
# produces target/release/libfs_ext4.a (static lib for FFI consumers)
#         + target/release/libfs_ext4.rlib
#         + target/release/mkfs_ext4 (the standalone formatter binary)
```

Cross-compile to a specific target the usual way:

```sh
cargo build --release --target aarch64-unknown-linux-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

Platform-specific packaging (macOS `lipo` for a universal static
archive, an Xcode `.xcframework`, deb/rpm/Homebrew formulae)
belongs in the consuming project. `am-fs-ext4` itself stays portable
cargo — no platform-specific build scripts.

### Using from C

Link `libfs_ext4.a` and include `fs_ext4.h`:

```c
#include "fs_ext4.h"

fs_ext4_fs_t *fs = fs_ext4_mount("/path/to/disk.img");
if (!fs) {
    fprintf(stderr, "%s\n", fs_ext4_last_error());
    return 1;
}

fs_ext4_attr_t attr;
if (fs_ext4_stat(fs, "/hello.txt", &attr) == 0) {
    printf("size=%llu mode=%o\n", attr.size, attr.mode);
}

fs_ext4_umount(fs);
```

See `examples/capi_demo.rs` for the Rust-side equivalent.

### Using from Rust

```toml
[dependencies]
am-fs-ext4 = "0.7"
```

```rust
use std::sync::Arc;

use fs_ext4::block_io::FileDevice;
use fs_ext4::Filesystem;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fs = Filesystem::mount(Arc::new(FileDevice::open("/path/to/disk.img")?))?;
    let ino = fs.lookup_path_bytes(b"/hello.txt")?;
    let attrs = fs.stat_ino(ino)?;
    println!("size={} mode={:o}", attrs.size, attrs.mode);
    Ok(())
}
```

### Testing

The test contract is a set of chore tasks, and CI runs exactly these:

```sh
chore siblings      # ../rust-fs-core and ../fs-linux-test-harness at their pinned refs
chore tools         # what the HOST needs: ripgrep, and the VM (Vagrant/QEMU/KVM)
chore fixtures      # build test-disks/*.img (kernel-made, in the harness VM)
chore test:unit     # the tests that need no tool, no fixture and no VM
chore test:images   # the tests that read a fixture but need no VM
chore test:oracle   # the driver writes, e2fsprogs reads back — in the VM
chore test:kernel   # the driver writes, the real kernel reads back — in the VM
chore test:lwext4   # a third implementation reads ours, and we read its — in the VM
chore test:wasm     # the browser build (wasm32-unknown-unknown), run under Node
chore test:vm       # the whole suite, compiled and run INSIDE the VM
chore test          # everything, as CI runs it
```

**Every Linux tool runs on Linux, in one place: the harness VM.**
`e2fsck`, `debugfs`, `mke2fs`, `dumpe2fs` and `tune2fs` are never run on
the host — not even on a Linux host that has them. e2fsprogs on a
workstation is whatever that machine happens to have (a keg-only Homebrew
formula on a Mac, a distribution build on Linux, a different version per
developer), and an oracle whose answer depends on which laptop asked is
not an oracle. So a single Debian guest, provisioned by
`scripts/vm-setup.sh`, holds them all;
`fs_ext4_test_support::oracle` is the only way a test reaches one, and
`tests/test_contract.rs` fails the suite if anything runs one here. **A
Mac needs no e2fsprogs installed at all.**

The same guest builds lwext4 — a third ext4 implementation, at the commit
`scripts/vm-setup.sh` pins — so `chore test:lwext4` can cross-validate
against something with no code lineage in common with either us or the
kernel. And it is the only place a filesystem is ever mounted: the kernel
oracles (`chore test:kernel`) loop-mount our images there, and
`chore fixtures` builds the kernel-made fixtures under `test-disks/`
(gitignored: loop mounts, xattrs, ACLs, inline data, htree directories —
recipes in `test-disks/guest-build-images.sh`). It is booted by
[fs-linux-test-harness](https://github.com/antimatter-studios/fs-linux-test-harness)
(Vagrant + QEMU, KVM on Linux, HVF on macOS; `chore vm:host:check` says
what the host is missing), and it is booted **once** for a whole run: the
first oracle call brings it up, every later call rides one shared SSH
connection (~0.1 s per tool invocation), and the run brings it down when it
ends: `scripts/test.sh` runs the suite inside a harness session, which also
releases the machine-wide VM slot however the run ends.

**We run the Linux tests on Linux.** On a Linux host, that host is Linux,
so `chore test` compiles and runs the suite natively (with the tools and
mounts in the guest, as above). On macOS nothing Linux-shaped runs
natively, so `chore test` runs `chore test:vm` instead: the harness mounts
this repository in the guest at the path the host knows it by, and the
suite is built and run in there with the toolchain
`rust-toolchain.toml` pins. The toolchain and the build directory live on
the VM's disk, so the first in-guest run pays a full build and later runs
are incremental. What runs natively on a Mac is what is genuinely
macOS-targeted: the crate compiled for macOS.

**No test skips.** A test that needs a fixture, a tool or the VM fails,
naming the task that provides it — a skipped test reads exactly like a
passing one.

`chore test` runs the suite through `./scripts/test.sh`, which gives every
invocation its own scratch directory and cleans it on exit. **That
directory is inside the repository** (`./tmp`, gitignored), on every
machine and on CI alike: the guest sees this repository and nothing else
of the host, so an image under `/tmp` or `$RUNNER_TEMP` would not exist
for the tool asked to read it. `FS_EXT4_TEST_TMPDIR` supplies an exact
directory instead, and is refused if it is outside the repository.

### Git hooks

One-time setup per clone, so every commit runs the same `cargo fmt` and
`cargo clippy` checks CI does. The hooks are
[github-guard](https://github.com/antimatter-studios/agent-skills)'s, installed
into `.git/hooks`, where a branch checkout cannot replace them.

github-guard ships in agent-skills under `.claude/skills/github-guard/`.
With agent-skills' `install-skill` skill set up, ask it to install
`github-guard`; that copies it to `~/.claude/skills/github-guard/`, and
then, from this repository:

```sh
~/.claude/skills/github-guard/install.sh .
```

Without `install-skill`, run the same script straight from a checkout of
agent-skills — it finds its hooks beside itself, so nothing needs copying
first:

```sh
git clone https://github.com/antimatter-studios/agent-skills.git ../agent-skills
../agent-skills/.claude/skills/github-guard/install.sh .
```

Bypass a single commit with `git commit --no-verify`.

## Disclaimer — use at your own risk

**Read this before pointing the crate at anything you care about.**

This is experimental filesystem code that reads *and writes* the
on-disk structures of live filesystems. Bugs in this class of
code can — and sooner or later will — corrupt or destroy data.
The MIT license above already contains the standard no-warranty
and limitation-of-liability clauses; this section restates them
in plain English so there is no ambiguity about what you are
agreeing to when you use the software.

**By using this software you accept that:**

- The author(s) and contributors provide this crate **as is**,
  with **no warranty of any kind**, express or implied — including
  but not limited to warranties of merchantability, fitness for a
  particular purpose, correctness, data integrity, durability,
  security, or non-infringement.
- The author(s) and contributors are **not liable** for any loss,
  damage, or expense of any kind arising out of or related to your
  use of the software. This explicitly includes (non-exhaustively)
  lost or corrupted data, corrupted filesystems, volumes that will
  no longer mount, hardware damage, downtime, lost revenue, missed
  deadlines, support costs, or any direct, indirect, incidental,
  special, consequential, or punitive damages — regardless of the
  legal theory under which such damages might be sought.
- You are **solely responsible** for backing up any data that
  could be touched by this software *before* running it. The
  only safe workflow when experimenting with an unofficial
  filesystem driver is: work on disk *images* or on *copies*,
  never on your only copy of anything irreplaceable.
- If that is not acceptable to you, **do not use this software**.

This disclaimer is a plain-English restatement of the license
terms above, not a separate license. The license terms apply in full.
