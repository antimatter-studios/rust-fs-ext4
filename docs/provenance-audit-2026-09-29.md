# rust-fs-ext4 — Code-Provenance Audit against Linux GPL sources

- Target: rust-fs-ext4 at commit `eb6c38c` (branch `fix/byte-exact-paths`). All `file:line` references below are to that commit; `main` has moved on since (see the status note).
- Compared against: Linux `master` @ 72d3fcf8 (2026-09-27): `fs/ext4`, `fs/jbd2`, `fs/unicode`, `include/linux/jbd2.h`. kernel.org `Documentation/filesystems/ext4/*.rst` was used as the public-spec baseline.
- Scope: all of `src/**` (38 files, ~34k lines), `include/fs_ext4.h`, `examples/`, `fuzz/`, `docs/`. Skipped: `target/`, `tmp/`, `.vm-share/`, disk images, and test code, which only uses e2fsprogs and the kernel as black-box oracles and is not a finding.
- This report contains **no Linux source text**. All similarities are described in prose.

> **Status (2026-09-29):** remediation is tracked in [PROVENANCE.md](../PROVENANCE.md). Of the correctness notes below, the external xattr block sort order (#379/#396) and the extent merge length cap (#387) were already fixed on `main` when this report was written; the casefold hash premise is #438.

---

## 1. Summary verdict

**The crate is overwhelmingly independent work. One module is a self-declared transcription of GPL code, and two doc comments quote a kernel C line verbatim.**

**Must re-implement (TRANSLATION):**
- `src/hash.rs`. The htree hashes (legacy, half-MD4, TEA, `str2hashbuf`, EOF clamp, default seed) were explicitly "transcribed" from `fs/ext4/hash.c`. The doc comment says so (line 78), and the commit message of 410abe0 / squash 00d9121 (#194/#196, 2026-09-17) says "The algorithms now transcribe fs/ext4/hash.c".
  - Even before that commit, the half-MD4 transform in the initial import (32061f5) used the kernel's particular algebraic form of the MD4 majority function, not RFC 1320's form.
  - The algorithm itself must be bit-identical for interop. The *expression* must be redone from a permissive source.

**Must remove (verbatim GPL text):**
- `src/inode.rs:180-182` and `src/inode.rs:452-454`. Both doc comments contain a fenced C block reproducing one line of `ext4_encode_extra_time` from `fs/ext4/ext4.h`, verbatim apart from renaming the time variable.
- `encode_extra_time` and `decode_extra_time` (inode.rs:162-190) are one-expression direct renderings of those kernel inline functions. That is trivial, but it is a **SUSPICIOUS** direct derivation. Re-derive them from the kernel.org `inodes.rst` description of the epoch bits.

**SUSPICIOUS (low risk, format-driven, but code shape follows a named kernel routine; restate or rewrite):**
- `jbd2.rs` `tag_bytes`: same decision ladder as the kernel's tag-size helper, including a quirk not in the public doc.
- `jbd2.rs` `checksum_declaration_error`: same three exclusivity rules in the same order as the kernel's superblock check.
- `xattr.rs` `xattr_entry_hash` and the block-hash fold: the constant names and fold rule are the kernel's; the algorithm is not in the public doc.
- `superblock.rs` `descriptor_location` and `group_head_metadata_blocks`: same branches as the kernel's descriptor-location and meta-bg helpers, including the local name `has_super` / `first`.
- `fs.rs` `is_htree_index_block` / `refuse_unverified_dir_block`: same classification rule as the kernel's directory-block reader.
- `fs.rs` + `alloc.rs` BLOCK_UNINIT bitmap rebuild: same step order as the kernel's block-bitmap initialiser.

**Everything else (extent tree, extent mutation, indirect maps, xattr layout, checksums, directories, htree lookup and split, allocators, mkfs, fsck, journal replay and writer, transactions, orphan recovery, casefold, ACL, inline data, C API) is FORMAT-FACT or INDEPENDENT.**
- Several of these have *designs that clearly differ* from the kernel: plan/apply mutation, no path arrays, no binary searches, 50/50 splits, repack-based punch, single-pass journal walk, post-hoc revoke filtering. That is strong evidence of independent authorship.
- They still carry **about 60 comments naming kernel-internal functions or source files**, which need rewording (list in §5).

**Third-party contributions:**
- Yuv: 4 commits, including abbb3db.
- zubvit: 42e42b5.
- Jarkko Sakkinen: 432f9d3.

All three were audited. No translated code was found in any of them. Yuv's abbb3db brought in kernel *behaviour* taken from reading `fs/jbd2/recovery.c`: restarting the sequence one past the first uncommitted transaction, and wrap-around transaction ordering. It also added kernel-source links and identifiers in comments and docs. This is behavioural, not expressive, but for a clean-room policy it should be restated in neutral terms.

**The README claim "The driver does not derive from any GPL/LGPL/AGPL source" (README.md:474-476) is currently false because of `hash.rs`, and arguably because of `inode.rs`'s time helpers.**

---

## 2. Findings table

Classes:
- **T** = TRANSLATION
- **S** = SUSPICIOUS
- **F** = FORMAT-FACT (comment rewording only)
- **I** = INDEPENDENT

### 2.1 Hashes, casefold, checksums, inode, xattr (audited directly)

| file:lines | function | class | evidence (paraphrased) | action |
|---|---|---|---|---|
| src/hash.rs:1-316 | whole module: `name_hash`, `clear_eof`, `legacy_hash`, `str2hashbuf`, `tea_transform`, `tea_hash`, `half_md4_transform`, `half_md4`, `init_state` | **T** | Doc comment line 78 calls it "a transcription of the kernel's ext4fs_dirhash (fs/ext4/hash.c)", and commit 410abe0 says the algorithms "now transcribe fs/ext4/hash.c". Specifics:<br>• Helper names are the kernel's: legacy hash, string-to-hash-buffer packer, half-MD4 transform.<br>• The legacy hash uses the kernel's two state variable names and its exact add/subtract clamp sequence.<br>• The half-MD4 round-2 boolean function uses the kernel's non-RFC "and plus masked-xor" form.<br>• The transform returns word 1, with a comment saying the kernel does the same.<br>• TEA runs the kernel's 16-round loop with the same two-accumulator update order.<br>• The EOF remap mirrors the kernel's post-processing, and a comment restates the kernel's empty-name loop condition.<br>The *packing* loop was restructured (a single loop with i%4), but the rest is a near line-by-line rendering. | Re-implement clean-room from a BSD reference (FreeBSD `sys/fs/ext2fs/ext2_hash.c` or lwext4 `src/ext4_hash.c`) plus RFC 1320, the TEA paper and debugfs vectors (§3). An engineer who has not read `hash.c` should write it. Keep `tests/htree_hash_vectors.rs` as the acceptance test; its 100+ vectors came from debugfs. |
| src/hash.rs:95-119 | `effective_version` (unsigned flag +3) | F | Superblock `s_flags` unsigned/signed hash is interop-required. The doc lists versions 3-5. | Reword the "kernel adds 3" comment. |
| src/casefold.rs:1-160 | `fold_name`, `siphash_2_4`, `casefold_name_hash` | I | NFD comes from the `unicode-normalization` crate and case folding from the `caseless` crate; both are MIT/Apache and use Unicode UCD data. No kernel `utf8data` tables are present. SipHash-2-4 is written from the Aumasson-Bernstein reference: the init constants are the paper's ASCII "somepseudorandomlygeneratedbytes", with standard SipRound rotations. Comments cite `fs/ext4/hash.c`, `fs/unicode/utf8-core.c` and a nonexistent `EXT4_CASEFOLD_HASH_SEED_SLOT`. | Reword comments (lines 7, 11, 22-28, 36). **Correctness note:** see §3.5. The module is dead code: no caller outside its own tests, and CASEFOLD is refused. |
| src/checksum.rs:36-44 | `linux_crc32c` | F | Wrapper converting crc32c-crate semantics to "raw, no final xor" CRC32C. The name and comment reference the kernel's `__crc32c_le`. | Rename (e.g. `crc32c_raw`) and reword. |
| src/checksum.rs:46-60 | `crc16` | F | Textbook reflected CRC-16/ARC (poly 0xA001 reflected), bit-at-a-time. The kernel uses a table. | Reword the "kernel's crc16() (lib/crc16.c)" comment and cite the CRC-16/ARC catalogue instead. |
| src/checksum.rs:62-100 | `group_desc_csum` | F | Recipe from `checksums.rst` / `group_descr.rst`. Comment says "Mirrors the kernel's ext4_group_desc_csum". | Reword. |
| src/checksum.rs:110-480 | superblock, dirent tail, dx tail, extent tail, xattr block, inode checksum helpers | F | Every recipe is documented in `checksums.rst`: seed + ino + gen + body, field zeroed, xattr block with LE64 block number. The dx-tail detail (count·8 bytes + tail reserved word + zero) and the inode "hi half only if i_extra_isize ≥ 4" rule go slightly beyond the doc, but are dictated by interop and verifiable with e2fsck. The code shape is not the kernel's. Comments cite `fs/ext4/dir.c::ext4_dirent_csum_set`, `fs/ext4/extents.c::ext4_extent_block_csum_set`, `fs/ext4/xattr.c::ext4_xattr_block_csum`, `ext4_dx_csum` and `EXT4_FITS_IN_INODE`. | Reword comments (lines 36-37, 46, 65, 119-120, 193, 289, 326, 403, 459-461). |
| **src/inode.rs:160-190, 448-456** | `decode_extra_time`, `encode_extra_time`, and a doc comment on the setter | **S + verbatim quote** | Lines 180-182 and 452-454 contain a fenced C block that is one line of the kernel's `ext4_encode_extra_time`, verbatim except for the variable name. Comments say "Matches ext4_decode_extra_time/ext4_encode_extra_time in fs/ext4/ext4.h". The Rust body renders the same expression. Commit 4b09f56 (#59, Christopher Thomas, 2026-09-04). | **Delete the C quotes.** Rewrite both helpers from `inodes.rst` ("Inode Timestamps": low 2 bits of `*_extra` extend the signed 32-bit seconds; upper 30 bits are nanoseconds). Express it as "epoch = (secs − sign-extended low 32 bits) / 2^32", in your own words. |
| src/xattr.rs:136-660 | `read_all`, `parse_entries*`, `split_qualified_name`, `plan_set/remove_in_inode_region`, `decode/encode_*`, `plan_set_in_external_block`, `plan_remove_from_external_block`, `get*`, `list_names` | I | Design is decode everything → sort → re-emit whole region/block. The kernel edits in place with its search/insert/memmove machinery. Layout facts come from `attributes.rst`. Comments naming `ext4_xattr_entry` (struct) are format names. | Reword "kernel stores/orders" comments (540, 659). **Correctness note:** the sort key is (name_index, name bytes). The kernel keeps external-block entries sorted by (name_index, **name length**, name bytes) and stops its search early on that order, so blocks written by this crate with names of different lengths can make the kernel miss an entry. Fix while re-implementing. |
| src/xattr.rs:666-735 | block-hash fold in `encode_external_block`; `xattr_entry_hash` | S | Algorithm not in public docs: `attributes.rst` only says "hash of name and value". The Rust uses the kernel's shift constants 5 and 16 with near-identical constant names, and the kernel's rule "a zero entry hash forces block hash 0". The comment names `ext4_xattr_rehash` / `ext4_xattr_hash_entry`. It is algorithmically forced (e2fsck validates it) and very short, but it was learned from the kernel. Note: the kernel also has a signed-char variant; the Rust uses only unsigned bytes. | Re-derive from lwext4's BSD `ext4_xattr_compute_hash` / `ext4_xattr_rehash`, or black-box from e2fsck. Reword comments. |
| src/ea_inode.rs:1-217 | `read_value_inode` etc. | F | Size cross-check = interop behaviour. Comment cites `ext4_xattr_inode_iget`. | Reword line 36 (and xattr.rs:91). |
| src/acl.rs:1-295 | ACL decoder | F | Compact ACL v1 layout (4/8-byte entries) is a format fact. The comment cites `fs/ext4/acl.h` as reference and uses kernel struct names. The code is its own. | Reword line 9 and cite a neutral description (FreeBSD `ext2_acl.h`, BSD) instead. |
| src/inline_data.rs, src/features.rs, src/superblock.rs (parse), src/bgd.rs | parsing / feature classification | F | Layouts per kernel.org docs. Comments cite `ext4_fill_super`, `ext4_check_descriptors`, `ext4_handle_clustersize`, `ext4_num_base_meta_clusters`, `descriptor_loc`, and constants `EXT4_MAX_CLUSTER_LOG_SIZE` / `EXT4_MIN_BLOCK_LOG_SIZE`. | Reword. |
| src/capi.rs, include/fs_ext4.h, examples/, fuzz/ | C ABI, demos, fuzz targets | I | No kernel logic. Names like `ext4_fopen` in tests/docs are lwext4 (BSD) API names, not Linux. | none |

### 2.2 Extent tree, indirect, truncate

| file:lines | function | class | evidence | action |
|---|---|---|---|---|
| extent.rs:1-199 | module constants, header/extent/index parse | F | Offsets, magic 0xF30A, the 32768 unwritten rule and max depth 5 all come from `blockmap.rst`. | none |
| extent.rs:214-330 | `lookup`, `lookup_verified` | I | Linear scan with a descent-cycle guard; no binary search. "Descend into first index when target precedes all keys" is a format consequence. Comment at 280 names `ext4_ext_binsearch_idx` / `EXT_FIRST_INDEX`. | Reword 274-282. |
| extent.rs:334-448 | `collect_all*`, `walk`, `map_logical*` | I | Recursive DFS; no kernel counterpart. | none |
| extent_mut.rs:31-130, 382-497 | encode/build/parse helpers | F | Serialization. | none |
| extent_mut.rs:134-207, 605-656 | `are_contiguous`, `plan_insert_extent`, sorted insert, overlap check | I | Decode → insert into Vec → merge → re-emit. No kernel merge-left/right ordering. | none. *Correctness:* merges have no 32768/32767 length cap (the cap exists only in `plan_initialize_range`). |
| extent_mut.rs:212-322 | `plan_promote_leaf` | I | Builds a fresh one-index root plus leaf; the kernel instead moves the root into a new block. | none |
| extent_mut.rs:537-602 | `plan_repack_tree` | I | Bottom-up bulk load; no kernel equivalent. | none |
| extent_mut.rs:669-991 | `descend_to_leaf`, `plan_insert_extent_deep` | I (key fix-up is F) | No path array; frames hold `{block, bytes}`. 50/50 splits; two-block root promotion. The "parent key = child's first key, propagate while first entry" invariant is e2fsck-enforced. Comment at 772 names `ext4_ext_correct_indexes`. | Reword 768-772. |
| extent_mut.rs:1000-1139 | split/merge/free/initialize range | I | List-index operations; length caps are format-derived. | none |
| indirect.rs, indirect_mut.rs | block-map read/write/free | I | Tier cascade with explicit arithmetic; cites kernel.org docs and Carrier. | none |
| file_mut.rs:28-385 | deep truncate (Yuv #169), write/grow/shrink planners | I | Left-to-right recursive walk; the kernel removes right-to-left iteratively. | none |
| fs.rs ~1161-2100, ~4700-4900 | truncate, fallocate, punch, zero-range, pwrite extent paths | I | Plan/apply; punch = collect/filter/repack; zero-range = punch + prealloc (the kernel does not do this). | none |

### 2.3 Journal / JBD2 / transactions / orphans

| file:lines | function | class | evidence | action |
|---|---|---|---|---|
| jbd2.rs:1-47, 274-337 | module doc, `JournalSuperblock::parse` | F | Layout per `journal.rst`. Line 3 cites `fs/jbd2/journal.c` + `include/linux/jbd2.h` as "Spec". | Cite `journal.rst` instead. |
| jbd2.rs:115-137 | `validate_plain_recovery` (Yuv) | I | Own policy. | none |
| jbd2.rs:185-198 | `checksum_declaration_error` | F/S (borderline) | Same three exclusivity rules in the same order as the kernel's superblock checksum check. The rules are interop-required but only partly documented. Christopher Thomas, #229. | Restate in own order and words; reword comments 180, 422. |
| **jbd2.rs:206-224** | `tag_bytes` | **S** | Same decision ladder as the kernel's tag-size helper, including the CSUM_V2 "+2 bytes" quirk that is **not** in `journal.rst`. Trivial logic. Christopher Thomas, #229. | Replace with an explicit (feature-set → size) table verified black-box against kernel/e2fsprogs-written journals; reword 206-210, 399-402. |
| jbd2.rs:200-204, 249-272 | `csum_seed`, tag / tail / commit checksums | F | Recipes in `journal.rst`. Comments name `j_csum_seed`, `jbd2_block_tag_csum_set`, `jbd2_descriptor_block_csum_set`, `jbd2_revoke_csum_set`, `jbd2_commit_block_csum_set`. | Reword. |
| journal.rs:144-279 | `walk` | I (code) | Single pass building a plan; none of the kernel's scan/revoke/replay passes or its variable names. Revokes filtered afterwards via HashMap. Fails the walk on a bad data-block checksum where the kernel skips. The *policy* for a bad descriptor/revoke checksum (torn tail vs corruption decided at commit) matches the kernel as an idea. | Reword 49, 112, 210. |
| journal.rs:109-136 | `filter_revoked`, `sequence_after` (Yuv) | F | Standard wrapping serial-number compare (RFC 1982-style). Comment names `tid_gt`. | Reword 132-133; cite RFC 1982 serial arithmetic. |
| journal.rs:343-458 | descriptor-tag / revoke-block parsing | F | `journal.rst`. Comments name `journal_tag_bytes`, `scan_revoke_records`. | Reword 13, 23, 430, 559. |
| journal_apply.rs:72-100 | `mark_journal_clean` (Yuv, abbb3db) | F (behavioural) | "Restart one past the first uncommitted transaction" is kernel behaviour, cited with the kernel function and its local-variable increment expression. Not in the public doc. | Reword 85-89. State the rationale ("a torn tail may already carry the next ID") without kernel identifiers. |
| journal_apply.rs (rest), journal_writer.rs, transaction.rs (incl. zubvit #147) | apply/replay, writer, transaction serialisation, escaping | I / F | Writer checkpoints each transaction at block 1 behind flushes. Transactions always SAME_UUID, revokes after data, `chunks()`-based multi-descriptor split. Escaping per doc. | Reword transaction.rs:343, journal_writer.rs:409/415. |
| fs.rs:317-474, 785-1200 | `mount_recovering`/`finish` lifecycle (Yuv), orphan chain & recovery | I / F | No kernel counterpart for the lifecycle. The orphan "bad inode" stop tests resemble the kernel's but are combined differently. | Reword 806, 846, 912, 6868. |
| docs/CHECKED-RECOVERY.md:44 | reference list (Yuv) | — | Links `torvalds/linux/.../fs/jbd2/recovery.c`. | Remove the link; keep only `journal.rst`. |

### 2.4 Directories, htree, allocation, mkfs, fsck, superblock

| file:lines | function | class | evidence | action |
|---|---|---|---|---|
| htree.rs:84-283 | root/node parse, `find_entry_for_hash`, `lookup_leaf_with` | I | Linear scan for the last hash ≤ target; no frames or the kernel's binary-search pointer names. | none |
| htree_mut.rs:66-311 | leaf read/pack, `plan_leaf_split`, dx-entry insert | I | Split by *count* at the median of sorted (hash, bytes), keeping equal hashes together, never setting the continuation bit. The kernel splits by size and sets the bit. Insert refuses duplicate hashes. | none |
| fs.rs:2735-2923, 6030-6215 | indexed add / leaf split / drop-index fallback | I | One index level; drops the index instead of splitting index nodes. Comments name `ext4_add_entry`, `dx_fallback`, `ext4_dx_add_entry`, `do_split`, `dx_insert_block`. | Reword 2841-2842, 6047, 6071, 6074. |
| fs.rs:5016-5095 | `is_htree_index_block`, `refuse_unverified_dir_block` | S (low) | Same classification rule as the kernel's directory-block reader (block 0, or first record spanning the block ⇒ index node; do not verify node-shaped blocks on linear read). Adds its own inode==0 test; different shape. Christopher Thomas, fac5c38 / 3ca628d. | Restate as a format rule; reword 5019, 5063, 5074. |
| fs.rs:2221-2300 + alloc.rs ~367-460 | BLOCK_UNINIT bitmap reconstruction | S (low) | Same step order as the kernel's bitmap initialiser, but that order is simply what BLOCK_UNINIT means. Code is run-list based. Comments name `ext4_init_block_bitmap`, `ext4_mark_bitmap_end`. | Reword alloc.rs:367, fs.rs:2283. |
| alloc.rs:330-640 | block/inode planners, `orlov_select_group` | I | First-fit from the parent group. The "Orlov" function is a simple score that shares nothing with the kernel's Orlov implementation. The module doc (24-29) misdescribes the kernel heuristic. | Optionally rename; fix the doc. |
| superblock.rs:474-531 | `descriptor_location`, `group_head_metadata_blocks` | S (format-driven) | Same branches as the kernel's descriptor-location and meta-bg-count helpers (including the 1 KiB / first_data_block==0 extra block), and same local names (`has_super`, `first`). Christopher Thomas, 1bb684d / 54f7c7e. | Rename locals and restate from `blockgroup.rst` (meta_bg section); reword 475, 512. |
| superblock.rs:118-458, 812-850 | `group_has_super`, `parse` validation, cluster checks | F | Different order and messages; own extra checks. | Reword 321, 347, 571, 812. |
| bgd.rs:~100-262 | `read_all`, `check_pointers_within` | F | Different structure from the kernel's descriptor check. | Reword 215. |
| dir.rs:150-395 | linear dirent add/remove | F | Classic ext2 slack-reuse technique. | none |
| mkfs.rs:95-640 (incl. Jarkko Sakkinen's `format_block_groups`) | formatter | I | Fixed-geometry classic layout; nothing resembles e2fsprogs' `ext2fs_initialize` / `ext2fs_allocate_tables`. | Reword 364 (`fs/ext4/bitmap.c::ext4_{block,inode}_bitmap_csum_set`), 707 (`ext4_get_journal_inode`). |
| bin/mkfs_ext4.rs, fsck.rs, verify.rs, path.rs | CLI, fsck/audit, path lookup | I | Own phases; no e2fsck message text except as quoted oracle output. | none |

---

## 3. Hash algorithms: public / permissive specifications for a clean re-implementation

**What kernel.org documents:** `directory.rst` ("Hash Tree Directories", the `dirhash` table) specifies only:
- the version codes: 0 legacy, 1 half-MD4, 2 TEA, 3/4/5 unsigned variants, 6 SipHash;
- that hashes are 31-bit with the low bit cleared;
- that the seed is `s_hash_seed`, and that `s_def_hash_version` / `s_flags` apply.

**It does not specify the algorithms.** So the algorithm bodies must come from permissively licensed code plus primary crypto references, validated by black-box vectors.

| algorithm | primary / permissive references | notes for the implementer |
|---|---|---|
| Legacy ("dx_hack_hash") | FreeBSD `sys/fs/ext2fs/ext2_hash.c` (BSD-2-Clause, `ext2_legacy_hash`); lwext4 `src/ext4_hash.c` (BSD, derived from FreeBSD) | Not a published algorithm; only BSD code and vectors. Signed vs unsigned char matters. |
| Half-MD4 | **RFC 1320** (MD4: F/G/H functions, round constants 0x5A827999 and 0x6ED9EBA1, per-round shift amounts, default IV 0x67452301/0xEFCDAB89/0x98BADCFE/0x10325476). The ext4 "half" schedule (8 words per round, 3 rounds, no length padding, output words 1 and 2) is in FreeBSD `ext2_hash.c` (`ext2_half_md4`, carries the RSA MD4 derivative notice) and lwext4 `ext4_hash.c`. | Use RFC 1320's majority form for G (lwext4 does). It is bit-equivalent to the kernel's form. Name-to-words packing, the length-derived pad word and the 32-byte chunking are in the BSD code. |
| TEA | **Wheeler & Needham, "TEA, a Tiny Encryption Algorithm"** (FSE 1994): the round function and delta 0x9E3779B9. The ext4 usage (16 cycles, the name block used as the key, adding back into the running state, 16-byte chunks) is in FreeBSD `ext2_hash.c` (`ext2_tea`) and lwext4. | |
| Final step (all three) | same BSD sources | Clear bit 0. If the result equals the EOF sentinel (0x7FFFFFFF shifted left by 1), step down by 2. |
| SipHash-2-4 (version 6, encrypted+casefold only) | **Aumasson & Bernstein, "SipHash: a fast short-input PRF"** (INDOCRYPT 2012) and the reference C implementation (CC0). ext4 context: `directory.rst` (encrypted+casefolded dirents carry hash/minor_hash); the fscrypt docs (`Documentation/filesystems/fscrypt.rst`) for key derivation. | Keyed by the per-directory fscrypt-derived key, **not** by `s_hash_seed`. Hash major = high 32 bits, minor = low 32 (verify black-box). |
| Casefold normalisation | **Unicode UCD**: `CaseFolding.txt` (status C+F full folding), `UnicodeData.txt` decomposition mappings, **UAX #15** (NFD). The version is set by `s_encoding` (`super.rst`: 1 = UTF-8, Unicode 12.1). | Pin to UCD 12.1 for exact kernel parity. The current crates track the latest Unicode version. |
| CRC32C / CRC16 | RFC 3720 §B.4 (CRC32C, Castagnoli); CRC-16/ARC in the Greg Cook CRC catalogue | ext4 uses "raw" CRC32C (no final inversion, seed passed through). The superblock/UUID seeds are in `checksums.rst`. |
| xattr entry/block hash | not publicly specified; lwext4 `src/ext4_xattr.c` (BSD) | Rolling shift-xor over name bytes, then value LE32 words; block hash folds the entry hashes; any zero entry hash ⇒ block hash 0. |

### 3.1 Test-vector oracle: `debugfs dx_hash` (tested locally, e2fsprogs 1.47.2)

Usage: `debugfs -R "dx_hash [-cv] [-h alg] [-s seed-uuid] [-e encoding] NAME" image`

| request | works? | example (seed `01234567-89ab-cdef-0123-456789abcdef`) |
|---|---|---|
| `-h legacy` / `-h 0` | yes | `hello` → 0x32252546, minor 0 |
| `-h half_md4` / `-h 1` | yes | `hello` → 0xa26e4a80, minor 0x97e5b7f7 |
| `-h tea` / `-h 2` | yes | `hello` → 0x6f5bb1a8, minor 0x231917c2 |
| unsigned variants | **only numerically**, as `-h 3`, `-h 4`, `-h 5` | `hé` gives v0 0xe7177860 / v3 0xcd937c64; v1 0xda3ece32 / v4 0x08a1bdb4; v2 0x07b59952 / v5 0x9b3eb2ec |
| `-h legacy_unsigned`, `-h half_md4_unsigned`, `-h tea_unsigned`, `-h siphash` (names) | **no** | Names not recognised: they are silently treated as legacy (0x32252546). Always pass numbers. |
| `-h 6` (SipHash) | **no** | "Directory hash unsupported". SipHash needs an fscrypt key; vectors must come from the SipHash paper's reference vectors plus a kernel-made encrypted+casefolded image. |
| `-c -e utf8` (casefold) | yes | `-h half_md4 -c -e utf8 HELLO` = plain `hello` = 0xa26e4a80. |

The `-c -e utf8` result shows that casefolded directories hash the *folded* name with the directory's normal hash (half-MD4 etc.), not with SipHash.

`tests/htree_hash_vectors.rs` already holds a debugfs-derived table for versions 0-5 across two seeds, with boundary lengths and high-bit bytes. That is the ready-made acceptance suite for a clean re-implementation.

### 3.2 Correctness note on `casefold.rs`

The module assumes casefolded directories use SipHash keyed by `s_hash_seed`. That is wrong:
- A non-encrypted casefolded directory uses its normal hash version over the NFD-casefolded name. The debugfs check above demonstrates this.
- SipHash is used only for *encrypted + casefolded* directories, keyed by fscrypt.

The module is currently unused (CASEFOLD is refused), so there is no live bug. A clean rewrite should implement "fold (UCD 12.1 NFD + full case fold) then run the ordinary htree hash", and validate it with `debugfs dx_hash -c -e utf8`.

---

## 4. History and authorship

- **History starts at 32061f5 (2026-04-18, "initial import from ext4-fskit@aaa63cf", Chris Thomas).** Earlier history is not in this repository. Code present at import (e.g. half-MD4 with the kernel-form G function, the htree lookup, the Orlov stub, indirect maps) is covered by this audit as it exists today; its pre-import history is described in PROVENANCE.md.
- **Authors:**
  - Chris Thomas / Christopher Thomas (3 email identities, about 460 commits).
  - Yuv `<1vivy@tutanota.com>`: 4 commits, 2026-09-26.
  - zubvit: 1 commit, 2026-09-27.
  - Jarkko Sakkinen: 1 commit, 2026-06-21, DCO sign-off.
- **Kernel-referencing content, when and who:**

| content | commit | date | author |
|---|---|---|---|
| hash.rs half-MD4 (kernel G form), names `str2hashbuf` / `half_md4_transform` | 32061f5 (import) | 2026-04-18 | Chris Thomas |
| xattr hash + rehash comment | 83926e2d / 21f3cbd1 | 2026-05-03 / 06-30 | Chris / Christopher Thomas |
| mkfs bitmap-csum comment citing `fs/ext4/bitmap.c` | f39e760 | 2026-05-03 | Chris Thomas |
| casefold.rs comments citing hash.c / utf8-core.c | c43aa4ce | 2026-05-28 | Chris Thomas |
| inode.rs verbatim C line + `ext4_{en,de}code_extra_time` | 4b09f56 (#59) | 2026-09-04 | Christopher Thomas |
| checksum.rs `linux_crc32c` / `crc16` kernel comments | 7533ddfa | 2026-09-16 | Christopher Thomas |
| **hash.rs "transcribe fs/ext4/hash.c"** | 410abe0 → squash 00d9121 (#194/#196) | 2026-09-17 | Christopher Thomas (+AI co-author) |
| extent.rs / extent_mut.rs kernel-function comments | a650c407 | 2026-09-17 | Christopher Thomas |
| ea_inode.rs `ext4_xattr_inode_iget` comment | 83d89b30 | 2026-09-19 | Christopher Thomas |
| jbd2 tag_bytes / checksum-exclusivity / kernel-function comments | #229 | Sep 2026 | Christopher Thomas |
| superblock descriptor_location, dir-block rule, uninit bitmap comments | 1bb684d, 54f7c7e, fac5c38, 3ca628d, 4924373, 309abd6 | 2026-08-25 … 09-28 | Christopher Thomas |
| JBD2 recovery behaviour + `recovery.c` link + `jbd2_journal_recover` / `tid_gt` comments | abbb3db (#146) | 2026-09-26 | **Yuv** (third party) |

- **Third-party code verdicts:**
  - Yuv abbb3db (recovery lifecycle), 10a149f (deep truncate), ddce073 (runtime provider), d5794d6 (uninit staging): independent code. abbb3db imports kernel behaviour and kernel references (see §2.3).
  - zubvit 42e42b5: independent.
  - Jarkko Sakkinen 432f9d3: independent.
- **Data tables:**
  - The repo contains no Unicode or casefold tables. Folding uses the `unicode-normalization` and `caseless` crates (MIT/Apache, generated from the Unicode UCD), not kernel `utf8data.c_shipped`.
  - Hash constants are the MD4 IV/round constants (RFC 1320), the TEA delta (TEA paper), SipHash constants (SipHash paper) and the two legacy-hash seeds plus multiplier. The last are only obtainable from code: the kernel, e2fsprogs or FreeBSD/lwext4 (BSD).
  - CRC32C comes from the `crc32c` crate; CRC16 is computed bitwise, with no table.
- **README/licensing:** README.md:474-476 says the driver "does not derive from any GPL/LGPL/AGPL source". Commit 1baa872 ("license sweep") shows an earlier cleanup, but kernel references were re-introduced afterwards. The README credits `yuoo655/ext4_rs` (MIT) as a research reference; that crate is itself modelled on lwext4.

---

## 5. Kernel-internal identifiers found in the repo (for a CI denylist)

These are function, macro, local-variable and source-path names internal to Linux (or e2fsprogs internals), as opposed to on-disk format names published in the kernel.org docs.

### 5.1 Denylist: fail CI on these in `src/`, `include/`, `docs/`, `examples/`, `fuzz/`

The check does not exist yet; adding it is item 7 of §7. When it is written,
it must exclude this report (and any later audit report under
`docs/provenance-audit-*.md`), which spells every denylisted name by design.

```
# source paths
fs/ext4/  fs/jbd2/  fs/unicode/  lib/unicode/  include/linux/  lib/crc16.c  torvalds/linux
# hash.c
ext4fs_dirhash  ext4fs_dirhash_casefold  __ext4fs_dirhash  dx_hack_hash  str2hashbuf  half_md4_transform  TEA_transform
utf8_casefold_hash  utf8_casefold  EXT4_CASEFOLD_HASH_SEED_SLOT
# checksum/crc
__crc32c_le  crc32c_le  ext4_group_desc_csum  ext4_dirent_csum_set  ext4_dx_csum  ext4_extent_block_csum_set
ext4_xattr_block_csum  ext4_block_bitmap_csum_set  ext4_inode_bitmap_csum_set  ext4_csum_seed  EXT4_FITS_IN_INODE
# inode/time
ext4_encode_extra_time  ext4_decode_extra_time
# extents
ext4_ext_correct_indexes  ext4_ext_binsearch_idx  EXT_FIRST_INDEX
# xattr
ext4_xattr_rehash  ext4_xattr_hash_entry  ext4_xattr_inode_iget  ext4_xattr_update_super_block
# namei/dir
ext4_add_entry  ext4_dx_add_entry  dx_fallback  do_split  dx_insert_block  __ext4_read_dirblock  make_indexed_dir  dx_probe
# super/balloc/orphan/symlink
ext4_fill_super  ext4_check_descriptors  descriptor_loc  ext4_num_base_meta_clusters  ext4_handle_clustersize
ext4_init_block_bitmap  ext4_mark_bitmap_end  ext4_get_journal_inode  ext4_orphan_cleanup  ext4_symlink
EXT4_MAX_CLUSTER_LOG_SIZE  EXT4_MIN_BLOCK_LOG_SIZE
# jbd2
jbd2_journal_recover  jbd2_journal_abort  jbd2_journal_write_metadata_buffer  jbd2_block_tag_csum_set
jbd2_descriptor_block_csum_set  jbd2_revoke_csum_set  jbd2_commit_block_csum_set  journal_check_superblock
journal_tag_bytes  do_one_pass  scan_revoke_records  need_check_commit_time  tid_gt  end_transaction  j_csum_seed
```

Also add the regex `` ```c `` (a C code fence) in `src/**/*.rs` doc comments.

### 5.2 Current occurrences (file:line)

| file | lines | identifiers |
|---|---|---|
| src/hash.rs | 78, 110, 124, 144, 187, 211, 260, 299, 482 | `ext4fs_dirhash`, `fs/ext4/hash.c`, `dx_hack_hash`, `str2hashbuf` (+fn name 153), `half_md4_transform` (fn 262), `dx_hash.c`, plus two comments paraphrasing kernel return-value and loop-condition details |
| src/casefold.rs | 7, 11, 28, 36 | `fs/ext4/hash.c::ext4fs_dirhash_casefold`, `fs/unicode/utf8-core.c`, `lib/unicode/utf8-core.c`, `utf8_casefold_hash`, `EXT4_CASEFOLD_HASH_SEED_SLOT` |
| src/checksum.rs | 36-37, 46, 65, 120, 193, 289, 326, 403, 459 | `__crc32c_le`, `lib/crc16.c`, `ext4_group_desc_csum`, `fs/ext4/dir.c::ext4_dirent_csum_set`, `ext4_dx_csum`, `fs/ext4/extents.c::ext4_extent_block_csum_set`, `fs/ext4/xattr.c::ext4_xattr_block_csum`, `EXT4_FITS_IN_INODE` (+fn name `linux_crc32c` used crate-wide) |
| src/inode.rs | 162, 178, 180-182, 452-454 | `ext4_decode_extra_time`, `ext4_encode_extra_time`, `fs/ext4/ext4.h`, **verbatim C line** |
| src/xattr.rs | 91, 671, 715 | `ext4_xattr_inode_iget`, `ext4_xattr_rehash`, `ext4_xattr_hash_entry` |
| src/ea_inode.rs | 36 | `ext4_xattr_inode_iget` |
| src/acl.rs | 9, 45 | `fs/ext4/acl.h`, `<linux/posix_acl.h>` |
| src/extent.rs | 280-281 | `ext4_ext_binsearch_idx`, `EXT_FIRST_INDEX` |
| src/extent_mut.rs | 772 | `ext4_ext_correct_indexes` |
| src/mkfs.rs | 364, 707 | `fs/ext4/bitmap.c::ext4_{block,inode}_bitmap_csum_set`, `ext4_get_journal_inode` |
| src/alloc.rs | 367 | `ext4_init_block_bitmap` |
| src/bgd.rs | 215 | `ext4_check_descriptors` |
| src/superblock.rs | 321, 347, 475, 512, 571, 799-800, 812 | `ext4_fill_super`, `descriptor_loc`, `ext4_num_base_meta_clusters`, `EXT4_MAX_CLUSTER_LOG_SIZE`, `EXT4_MIN_BLOCK_LOG_SIZE`, `ext4_handle_clustersize` |
| src/jbd2.rs | 3, 93, 180, 200, 206, 249, 258, 266, 399, 402, 422 | `fs/jbd2/journal.c`, `include/linux/jbd2.h`, `jbd2_journal_abort`, `journal_check_superblock`, `j_csum_seed`, `journal_tag_bytes`, `jbd2_block_tag_csum_set`, `jbd2_descriptor_block_csum_set`, `jbd2_revoke_csum_set`, `jbd2_commit_block_csum_set` |
| src/journal.rs | 13, 23, 49, 112, 133, 210, 430, 559 | `fs/jbd2/journal.c`, `journal_tag_bytes`, `do_one_pass`, `scan_revoke_records`, `tid_gt`, `need_check_commit_time` |
| src/journal_apply.rs | 86-87 | `jbd2_journal_recover`, `info.end_transaction` |
| src/transaction.rs | 343 | `jbd2_journal_write_metadata_buffer`, `do_one_pass` |
| src/fs.rs | 806, 846, 912, 6868; 1285, 3982; 2283; 2841-2842; 3293; 5019, 5063, 5074; 6047; 6071; 6074 | `ext4_orphan_cleanup`; `ext4_symlink`; `ext4_mark_bitmap_end`; `ext4_add_entry`, `dx_fallback`; `ext4_xattr_update_super_block`; `__ext4_read_dirblock`; `ext4_dx_add_entry`; `do_split`, `dx_insert_block`; `dx_fallback` |
| docs/CHECKED-RECOVERY.md | 44 | link to `torvalds/linux/.../fs/jbd2/recovery.c` |
| docs/format-conformance-gaps.md | 48 | `fs/ext4/hash.c` |

### 5.3 Allowlist: format names that are fine

These appear in the kernel.org docs: `ext4_super_block`, `ext4_group_desc`, `ext4_inode`, `ext4_extent{,_header,_idx,_tail}`, `ext4_dir_entry{,_2,_tail}`, `dx_root`, `dx_root_info`, `dx_entry`, `dx_node`, `dx_tail`, `ext4_xattr_entry`/`_header`, `journal_header_t`, `journal_superblock_t`, `journal_block_tag{,3}_t`, all `s_*`/`i_*`/`ee_*`/`eh_*`/`h_*`/`e_*`/`t_*`/`r_*` field names, `EXT4_*_FL` inode flags, `EXT4_FEATURE_*`/`JBD2_FEATURE_*` bits, `EXT2_FLAGS_UNSIGNED_HASH`, `EXT_INIT_MAX_LEN`, and lwext4 API names (`ext4_fopen`, `ext4_mount`, ...).

---

## 6. Recommended AGENTS.md rule text

This is proposed text, to be adopted together with the check in §5.1. Its
sentence about CI enforcing `scripts/check-provenance.sh` becomes true only
once that script exists; until then the check is open (see `PROVENANCE.md`,
Remediation status).

```markdown
## Clean-room rule: no GPL source as input

This crate must stay permissively licensable. Therefore:

- **Never open, fetch, paste, summarise or "transcribe" Linux kernel source**
  (`fs/ext4`, `fs/jbd2`, `fs/unicode`, `include/linux/*`, `lib/*`) or
  e2fsprogs/e2fsck library source (`lib/ext2fs`, `e2fsck/`, `debugfs/` code),
  in this repo or in any tool/agent session that writes to it. This applies to
  code, comments, commit messages, docs and PR descriptions.
- **Allowed sources:** the kernel.org ext4/jbd2 *documentation*
  (`Documentation/filesystems/ext4/*.rst`, rendered at kernel.org/doc),
  RFCs and papers (RFC 1320 MD4, RFC 3720 CRC32C, TEA, SipHash), the Unicode
  UCD, Carrier's *File System Forensic Analysis*, and permissively licensed code
  (lwext4, FreeBSD `sys/fs/ext2fs`, MIT/BSD/Apache crates), with attribution.
- **Allowed oracles (black box only):** running `mke2fs`, `e2fsck`, `debugfs`
  (e.g. `dx_hash`), `dumpe2fs` and a real kernel mount, and comparing bytes
  or outputs. Record the *observed behaviour*, never the tool's source.
- **When the docs are silent**, derive behaviour from oracle experiments and
  describe it in your own words as a format rule ("a revoke hides the same or
  earlier transaction", "sequence numbers compare by wrapping difference"),
  never as "what kernel function X does".
- **Do not name kernel-internal functions, macros, locals or source paths** in
  code, comments or docs. On-disk structure and field names from the kernel.org
  docs are fine. CI enforces a denylist (`scripts/check-provenance.sh`);
  C code fences in Rust doc comments are rejected.
- **Third-party contributions** must follow the same rule. Reviewers reject
  PRs that link to or cite kernel source.
- An agent that already has kernel source in context must not write code for
  this repo in that session.
```

---

## 7. Recommended remediation order

1. **hash.rs**: clean-room rewrite by someone who has not read `fs/ext4/hash.c`, from the BSD/RFC/paper sources in §3, gated by `tests/htree_hash_vectors.rs` and `live_debugfs_agrees`. Remove the "transcription" wording.
2. **inode.rs**: delete both C quotes; re-derive the extra-time helpers from `inodes.rst`.
3. **xattr hash**: re-derive from a BSD reference or black-box from e2fsck. (The external-block sort order is already fixed on `main`, #396.)
4. **jbd2.rs**: rewrite `tag_bytes` as a verified table; restate `checksum_declaration_error`.
5. **superblock.rs, fs.rs, alloc.rs**: rename the kernel-named locals in the SUSPICIOUS items; restate as format rules.
6. Reword every comment in §5.2. Drop kernel-source links from docs. Update README:474-476 only after items 1-3 land.
7. Add the §5.1 denylist as a CI check and the §6 text to AGENTS.md.
