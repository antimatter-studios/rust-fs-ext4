# The three API layers

The drivers in this family each expose the same three layers, so a caller
can open, inspect and recover any of their volumes in the same terms:

| Layer | What it is for | Trusts the volume? |
|---|---|---|
| **Volume** | Open a source, its size and geometry, the superblock, health | No: it reports what it finds |
| **Primitives** | Read-only, damage-tolerant access to each on-disk structure, plus the salvage verbs | No: every read says how far it can be believed |
| **Files** | Browse, read and copy through the namespace of a healthy volume | Yes: a fault refuses the read |

The shared traits for these layers live in rust-fs-core. This page maps what
this crate already has onto them, and names what each layer still needs.
The salvage verbs follow the family's driver convention: find candidates,
walk extents, rebuild the tree, report damage, read by extent, and
materialize from a plan.

## Volume

| Need | Today |
|---|---|
| Open a source | `Filesystem::mount` / `mount_lazy` / `mount_with_runtime` over a `BlockDevice`; `fs_core_bridge::CoreDevice` adapts any `fs_core::BlockDevice`; C ABI `fs_ext4_mount*` |
| Probe without mounting | `Superblock::read` / `Superblock::parse` (magic and sanity checks) |
| Geometry | `Superblock::block_size`, `block_group_count`, `blocks_per_group`, `inodes_per_group`, `desc_size`, `filesystem_bytes`; C ABI `fs_ext4_get_volume_info` |
| Superblock | `Superblock` (parsed fields plus the raw 1024 bytes) |
| Health | `Superblock::is_clean`, the `RECOVER` incompat flag, `verify::verify`, `fsck::audit`; and, without trusting the primary, `salvage::structure_damage` |

A mount refuses what it cannot trust: a bad superblock or descriptor
checksum, an unsupported incompat feature, a pointer outside the volume.
That is right for the file layer and wrong for recovery, which is why the
volume layer's health and geometry also come from the salvage reads below
when the primary superblock is damaged.

## Primitives

Everything here is in `src/salvage/`, reads through `fs_core::BlockRead`
(which has no write method, so nothing here can change the source), and
returns what it found with an `Integrity` verdict and `Damage` values
instead of refusing.

| Structure | Function | Notes |
|---|---|---|
| Superblock and backups | `read_superblock_at`, `superblock_copies`, `find_superblock` | Every copy the layout names, sound or not; the geometry comes from the primary, else the first sound backup, else a probe of the default backup groups |
| Group descriptors and their backup tables | `group_descriptors(source, sb, copy)`, `descriptor_table_copies` | Copy 0 is the primary; `META_BG` and the table after each superblock copy are both handled; a descriptor whose checksum fails is kept and marked |
| Inodes | `read_inode` | Located through any copy of the descriptors; checksum judged, never trusted |
| Damage to the above | `structure_damage` | Faulty and disagreeing copies, and the state the superblock records |
| Extent trees and indirect blocks | `map_inode` | Every run reached, the map blocks, inline data, and where and why the walk stopped (bad header, pointer out of range, a block reached twice, unreadable); a map that reaches fewer blocks than `i_blocks` counts is broken |
| Directories | `scan_dir_block`, `read_directory` | Linear, inline and hashed directories; entries recovered past a broken `rec_len`, and deleted entries left in a record's slack |
| Journal | `scan_journal` | Every copy of a filesystem block the ring still holds, from every transaction found, committed or not, checksummed where the journal keeps tag checksums; older copies of a damaged block in place |

The salvage verbs, built on the structures above:

| Verb | ext4 terms | Where |
|---|---|---|
| Find candidates | Every inode-table slot that is not all zeros, live or deleted, and every superblock copy | `Ext4Salvage::find_candidates` |
| Walk extents | An inode's extent tree or indirect block map, as source byte ranges in file order, holes and inline bytes kept | `Ext4Salvage::walk_extents` |
| Rebuild the tree | Directory entries as parent claims, each with its name; a directory's own `..` as a claim too; orphans and conflicts kept | `Ext4Salvage::rebuild_tree` |
| Report damage | The superblock copies and descriptor tables, then each live inode: its checksum, its map, a directory's blocks | `Ext4Salvage::report_damage` |
| Read by extent | Raw source bytes, no file object needed | `Ext4Salvage::read_extent` |
| Materialize from a plan | Source extents, zeros, literal bytes and block runs written to a caller's sink; an unreadable block becomes zeros and is recorded | `Ext4Salvage::materialize`, `ExtentWalk::plan` |

What the kernel leaves behind decides what deletion recovery can find, and
the oracle tests pin it: a deleted inode keeps its mode and gets `i_dtime`,
but is truncated first, so its map is empty; a linear directory's removed
entry is zeroed; a hashed directory's split leaves old copies of the moved
entries, which the scan finds.

## Files

| Need | Today |
|---|---|
| Look up a path | `Filesystem::lookup_path_bytes`, `lookup_at`, `path::lookup` |
| List a directory | `Filesystem::read_dir_ino`, C ABI `fs_ext4_dir_open` / `dir_next` |
| Metadata | `Filesystem::stat_ino`, `read_inode_verified`, C ABI `fs_ext4_stat*` |
| Read a file | `Filesystem::read_ino`, `file_io::read*`, C ABI `fs_ext4_pread_ino` / `read_file` |
| Symlinks, xattrs | `read_link`, `xattr`, C ABI `fs_ext4_readlink*`, `fs_ext4_getxattr` / `listxattr` |
| Copy out | The `fs.ext4 export` tool (rust-fs-core's shared transfer) |

The write path (`apply_*`, `journal_writer`, `mkfs`) is this crate's own and
belongs to none of the three layers.

## Moving onto the shared traits

When rust-fs-core publishes the traits, this crate implements them over the
functions above, and keeps today's API for one release as a deprecated
wrapper. Until then the salvage types here (`Extent`, `Integrity`, `Damage`,
`DamageKind`, `Severity`) have the shapes the convention gives them, so the
change is a conversion, not a rewrite.
