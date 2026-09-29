# Provenance

This document records where the code in this crate comes from, the rules it
is written under, and the results of provenance audits. It exists so the
crate's permissive licence can be relied on: anyone can check how the code
came to be and what was done when a problem was found.

## History

- **Origins in DiskJockey.** The driver started life as the ext4 support
  inside [DiskJockey](https://github.com/antimatter-studios/diskjockey), a
  macOS application that mounts disk images and remote storage as Finder
  volumes through FSKit. It was developed in a research repository,
  `ext4-fskit`, alongside the DiskJockey FSKit extension.
- **Extraction as a standalone library (2026-04-18).** The driver was
  extracted from `ext4-fskit@aaa63cf` into this repository as a generic,
  host-independent Rust crate with a C ABI. Commit `32061f5` ("initial import
  from ext4-fskit@aaa63cf") is the first commit here; history before the
  extraction is not part of this repository.
- **Since then** the crate has been developed here as an independent library,
  with its own releases, and is consumed by DiskJockey like any other user.

Everything in the crate today, including code that arrived with the initial
import, is covered by the audits below. They examine the code as it exists,
not only its history.

## Licence and sources

The crate is MIT-licensed and has no GPL, LGPL or AGPL dependencies.

Code is written from public, permissively usable sources only:

- the ext4 and JBD2 on-disk format documentation published at
  kernel.org (`Documentation/filesystems/ext4`);
- RFCs and papers (RFC 1320 MD4, CRC32C, the TEA paper, the SipHash paper)
  and the Unicode Character Database;
- permissively licensed implementations (BSD/MIT/Apache), with attribution;
- black-box oracles: running `mke2fs`, `e2fsck`, `debugfs`, `dumpe2fs` and a
  real Linux mount, and comparing their outputs and the bytes they write.
  Observing what a tool does is not copying its source.

Linux kernel and e2fsprogs *source code* is not a permitted input.

## Audit of 2026-09-29

A full provenance audit compared every module against the Linux `fs/ext4`,
`fs/jbd2` and `fs/unicode` sources, as of commit `72d3fcf8` (2026-09-27). The
report, [docs/provenance-audit-2026-09-29.md](docs/provenance-audit-2026-09-29.md),
describes every similarity in prose and contains no kernel source text.

**Result.** The crate is overwhelmingly independent work. Its extent tree,
indirect blocks, xattr layout, checksums, directories and htree, allocator,
mkfs, fsck, journal replay and writer, orphans, casefold folding, ACLs and C
API differ from the kernel in design, and match it only where the on-disk
format requires. Contributions from outside contributors were audited as
well and contain no translated code.

**Findings that must be remediated:**

| Finding | What was found | Remediation |
|---|---|---|
| `src/hash.rs` | The htree name hashes (legacy, half-MD4, TEA) are a translation of the kernel's implementation into Rust, as the module's own documentation and commit `410abe0` state. | Clean-room re-implementation from BSD references, RFC 1320 and the TEA paper, verified against the existing `debugfs`-generated vectors in `tests/htree_hash_vectors.rs`. |
| `src/inode.rs` | Two doc comments quote one line of kernel C verbatim (the extra-time encoding), and the timestamp helpers implement that expression. | Remove the quotes; re-derive the helpers from the kernel.org inode timestamp documentation. |

**Findings to restate for a clean record** (similar in structure to a kernel
routine, but dictated by the format): the xattr entry/block hash, the JBD2
tag-size and checksum-declaration checks, superblock descriptor-location
helpers, index-block classification, and the `BLOCK_UNINIT` bitmap rebuild.
About 60 comments and docs name kernel functions or link to kernel source;
they will be reworded to cite the format documentation or observed oracle
behaviour instead.

**Correctness notes from the audit:** the external xattr block sort order
(#379, fixed in #396) and the extent merge length cap (#387) were already
fixed on `main`; the casefold hash premise is #438.

## Remediation status

| Item | Status |
|---|---|
| Clean-room rewrite of `src/hash.rs` | open |
| Remove kernel C quotes and re-derive timestamp helpers in `src/inode.rs` | open |
| Restate the format-driven routines listed above | open |
| Reword comments and docs that name kernel internals | open |
| Clean-room rule in `AGENTS.md` and a CI denylist check | open |

Until the first two items are complete, the README's statement that the driver
"does not derive from any GPL/LGPL/AGPL source" does not hold for
`src/hash.rs` and the two quoted lines in `src/inode.rs`. This document will be
updated as each item lands.
