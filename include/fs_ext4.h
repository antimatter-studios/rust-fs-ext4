/*
 * fs_ext4.h — C ABI for the ext4rs pure-Rust ext4 driver.
 *
 * Link against libfs_ext4.a and #include this header. NULL / -1 / 0
 * failure sentinels with thread-local error details available via
 * fs_ext4_last_error() / fs_ext4_last_errno().
 *
 * PATHS ARE BYTES. Every `const char *path` is read as the bytes up to
 * its NUL and compared byte for byte against the directory entry names,
 * which ext4 stores with no encoding. Never decoded: a name that is not
 * UTF-8 (a latin-1 "caf\xe9.txt") is reachable by exactly its bytes, as
 * fs_ext4_dir_next reports them. UTF-8 is a byte string too, so a UTF-8
 * caller sees no difference. A path naming no file is ENOENT, one longer
 * than 4096 bytes is ENAMETOOLONG, a NULL one is EINVAL; "" "/" and "//"
 * name the root. A consumer whose own namespace requires valid UTF-8
 * should escape such a name REVERSIBLY, so it can be turned back into
 * these bytes. Extended-attribute names are text: not UTF-8 is EINVAL.
 *
 * MIT License — see LICENSE
 */

#ifndef FS_EXT4_H
#define FS_EXT4_H

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>
#include <sys/types.h>   /* mode_t, uid_t, gid_t */

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handle to a mounted ext4 filesystem */
typedef struct fs_ext4_fs fs_ext4_fs_t;

/* File type enumeration (matches ext4 dir entry types) */
typedef enum {
    FS_EXT4_FT_UNKNOWN  = 0,
    FS_EXT4_FT_REG_FILE = 1,
    FS_EXT4_FT_DIR      = 2,
    FS_EXT4_FT_CHRDEV   = 3,
    FS_EXT4_FT_BLKDEV   = 4,
    FS_EXT4_FT_FIFO     = 5,
    FS_EXT4_FT_SOCK     = 6,
    FS_EXT4_FT_SYMLINK  = 7,
} fs_ext4_file_type_t;

/* File/directory attributes.
 *
 * nsec fields are sub-second nanoseconds (0..999_999_999).  They are zero
 * when the on-disk inode's i_extra_isize is too small to hold the extra
 * timestamp words (ext2/ext3 inodes, or very old ext4 images).
 *
 * inode_flags mirrors the on-disk e2 flags word (FS_IOC_GETFLAGS convention):
 *   0x00000010  EXT4_IMMUTABLE_FL
 *   0x00000020  EXT4_APPEND_FL
 *   0x00000040  EXT4_NODUMP_FL
 *   0x00010000  EXT4_ENCRYPT_FL
 *   0x00020000  EXT4_CASEFOLD_FL
 *   …(full list in <linux/fs.h>)
 */
typedef struct {
    uint32_t inode;
    uint16_t mode;          /* POSIX mode bits */
    uint32_t uid;
    uint32_t gid;
    uint64_t size;
    /* Seconds since the Unix epoch, signed and 64-bit.
     *
     * Widened from uint32_t in 0.6.0. ext4's on-disk base field is a
     * SIGNED 32-bit value which the matching *_extra field extends by
     * two further bits, so the real range is roughly 1901..2446. A
     * uint32_t truncated everything past 2038 and turned every
     * pre-1970 date into a far-future one.
     *
     * THIS CHANGES THE STRUCT LAYOUT. Recompile any consumer. */
    int64_t  atime;
    int64_t  mtime;
    int64_t  ctime;
    int64_t  crtime;        /* Creation time (ext4 extra) */
    uint16_t link_count;
    fs_ext4_file_type_t file_type;
    /* Sub-second timestamp precision (added in v0.3) */
    uint32_t atime_nsec;
    uint32_t mtime_nsec;
    uint32_t ctime_nsec;
    uint32_t crtime_nsec;
    /* Inode flags (e2_flags / FS_IOC_GETFLAGS, added in v0.3) */
    uint32_t inode_flags;
    /* i_generation — monotone counter for NFS stale-handle detection */
    uint32_t generation;
    /* i_blocks in 512-byte units — matches st_blocks from stat(2) */
    uint64_t blocks_512;
} fs_ext4_attr_t;

/* Directory entry (returned during iteration) */
typedef struct {
    uint32_t inode;
    uint8_t  file_type;     /* fs_ext4_file_type_t */
    uint8_t  name_len;
    char     name[256];     /* null-terminated */
} fs_ext4_dirent_t;

/* Volume information */
/*
 * Snapshot of everything in the on-disk superblock that a UI / CLI
 * consumer might want to surface. Layout is part of the FFI ABI; new
 * fields land at the end so existing zero-init memcpy callers stay
 * valid. Field-by-field documentation lives in src/capi.rs alongside
 * the rust definition.
 */
typedef struct {
    /* ----- Identity ----- */
    char     volume_name[16];     /* NUL-terminated, <= 16 bytes */
    uint8_t  uuid[16];            /* raw 16-byte UUID */
    char     last_mounted[64];    /* NUL-terminated last-mount path, or "" */

    /* ----- Sizing ----- */
    uint32_t block_size;
    uint64_t total_blocks;
    uint64_t free_blocks;
    uint64_t reserved_blocks;     /* root-reserve blocks */
    uint32_t total_inodes;
    uint32_t free_inodes;
    uint16_t inode_size;
    uint32_t first_inode;         /* first non-reserved inode (s_first_ino) */
    uint32_t blocks_per_group;
    uint32_t inodes_per_group;

    /* ----- Provenance + capabilities ----- */
    uint32_t creator_os;          /* 0=Linux 1=Hurd 2=Masix 3=FreeBSD 4=Lites */
    uint32_t rev_level;
    uint16_t minor_rev_level;
    uint32_t feature_compat;
    uint32_t feature_incompat;
    uint32_t feature_ro_compat;
    uint16_t desc_size;           /* block-group descriptor size: 32 or 64 */
    uint8_t  default_hash_version;

    /* ----- Lifecycle / health ----- */
    uint16_t state;               /* s_state bit field */
    uint16_t errors_behavior;     /* s_errors: 1=continue 2=remount-ro 3=panic */
    uint32_t last_mount_time;     /* unix epoch seconds */
    uint32_t last_write_time;     /* unix epoch seconds */
    uint32_t last_check_time;     /* unix epoch seconds */
    uint32_t check_interval;      /* seconds between forced fsck; 0 = disabled */
    uint16_t mount_count;
    uint16_t max_mount_count;     /* 0 = unlimited */
    uint16_t def_resuid;
    uint16_t def_resgid;

    /*
     * 1 if the filesystem was NOT cleanly unmounted last time it was used
     * (captured from the on-disk s_state field at mount time, before any
     * journal replay the driver may perform). 0 if clean. Consumers should
     * surface a dirty value to the user and run fsck / journal replay
     * before permitting writes. Derived from `state` for caller convenience.
     */
    uint8_t  mounted_dirty;
} fs_ext4_volume_info_t;

/* ---- Block device callback interface ---- */

/*
 * Callback for reading blocks from the device.
 * Must read exactly `length` bytes at `offset` into `buf`.
 * Returns 0 on success, non-zero on error.
 * `context` is the opaque pointer passed to fs_ext4_mount_with_callbacks.
 *
 * Alignment: on a mount whose `cfg->block_size` is greater than 1, both
 * `offset` and `length` are always multiples of `block_size` (see
 * fs_ext4_blockdev_cfg_t). With `block_size` 0 or 1 they are whatever the
 * engine asks for, with no alignment at all: the superblock alone is read
 * as 1024 bytes at offset 1024, and metadata writes can be smaller than
 * a filesystem block.
 */
typedef int (*fs_ext4_read_fn)(void *context, void *buf,
                                   uint64_t offset, uint64_t length);

/*
 * Callback for writing blocks to the device.
 * Must write exactly `length` bytes from `buf` starting at `offset`.
 * Returns 0 on success, non-zero on error.
 * The same alignment contract as fs_ext4_read_fn applies.
 *
 * Optional — set to NULL when mounting read-only via
 * fs_ext4_mount_with_callbacks. Required (must be non-NULL) when mounting
 * read-write via fs_ext4_mount_rw_with_callbacks.
 */
typedef int (*fs_ext4_write_fn)(void *context, const void *buf,
                                    uint64_t offset, uint64_t length);

/*
 * Optional flush/fsync callback. Invoked when the driver wants pending
 * writes pushed to stable storage. May be NULL — the driver then treats
 * flush as a no-op (this is what FSKit's FSBlockDeviceResource already
 * does, as it batches synchronisation at a higher layer).
 */
typedef int (*fs_ext4_flush_fn)(void *context);

/*
 * Block device parameters for callback-based mounting.
 *
 * NOTE: `write` and `flush` were appended at the tail of the struct in
 * v0.1.3 to keep backward-compatible binary layout with v0.1.2 consumers.
 * Existing read-only callers that memset/zero-init their config are
 * unaffected — fs_ext4_mount_with_callbacks ignores both new fields and
 * always mounts read-only, regardless of what's in `write` / `flush`.
 */
typedef struct {
    fs_ext4_read_fn read;
    void   *context;     /* Passed to callbacks (e.g. FSBlockDeviceResource pointer) */
    uint64_t size_bytes; /* Total device/partition size */
    /*
     * On the mount entry points: the device's sector size — the unit it
     * requires every request's offset and length to be a multiple of
     * (e.g. 512 or 4096). When it is greater than 1 the driver sends the
     * callbacks only such requests: an unaligned read is widened to the
     * enclosing sectors, an unaligned write becomes a read-modify-write of
     * its first and last sectors, and an aligned request passes through
     * unchanged. The mount fails with EINVAL if it is not a power of two,
     * does not divide `size_bytes`, or is larger than the filesystem block
     * size. 0 or 1: no alignment; requests are passed through byte-granular.
     *
     * On fs_ext4_mkfs it means something else: the filesystem block size to
     * format with (0 selects the default).
     */
    uint32_t block_size;
    fs_ext4_write_fn write; /* NEW in v0.1.3; NULL if read-only */
    fs_ext4_flush_fn flush; /* NEW in v0.1.3; NULL = flush is a no-op */
} fs_ext4_blockdev_cfg_t;

/* ---- Lifecycle ---- */

/*
 * Mount an ext4 filesystem from the given device/image path.
 * Uses direct POSIX I/O. Returns NULL on failure. Read-only.
 */
fs_ext4_fs_t *fs_ext4_mount(const char *device_path);

/*
 * Mount an ext4 filesystem using callback-based I/O.
 * Use this from sandboxed environments (e.g. FSKit extensions)
 * where direct device access is not available.
 * Returns NULL on failure. Read-only — `cfg->write` / `cfg->flush` are
 * ignored even if non-NULL. Use fs_ext4_mount_rw_with_callbacks for
 * read-write mounts.
 */
fs_ext4_fs_t *fs_ext4_mount_with_callbacks(
    const fs_ext4_blockdev_cfg_t *cfg);

/*
 * Mount via an FsCoreDevice handle from a sister crate
 * (`qcow2_open` from am-img-qcow2, `partitions_open_slice` from
 * am-partitions, `fs_core_file_open` from am-fs-core).
 *
 * Single entry point — RO vs RW is decided by the inner device's
 * `is_writable()`, so callers don't need a `_rw` variant.
 *
 * The underlying device's reference count is incremented internally
 * (the handle itself is not retained); the caller
 * still owns their *FsCoreDevice and frees it via
 * `fs_core_device_close`. Closing the resulting fs_ext4_fs_t via
 * `fs_ext4_umount` drops the mount's own reference.
 *
 * Forward declared FsCoreDevice — full definition in `fs_core.h`.
 *
 * Returns NULL on failure; use fs_ext4_last_error() / fs_ext4_last_errno()
 * for detail.
 */
struct FsCoreDevice;
fs_ext4_fs_t *fs_ext4_mount_with_fs_core_device(struct FsCoreDevice *handle);

/*
 * Same as fs_ext4_mount_with_fs_core_device but defers journal replay.
 * Use this when the embedding context cannot tolerate replay running
 * synchronously inside the mount call — typically because the device
 * handle isn't fully writable until the mount call has returned.
 *
 * After mount, call `fs_ext4_replay_journal_if_dirty(fs)` once the
 * underlying write path is ready. Until journal replay runs, the
 * mounted state may reflect a partially-applied journal — readers
 * see what's on disk, not the post-replay view. Writes through the
 * crate will fail until replay completes.
 *
 * Same handle-ownership semantics as fs_ext4_mount_with_fs_core_device:
 * the underlying device's reference count is incremented internally
 * (the handle itself is not retained); the caller
 * still owns their *FsCoreDevice and frees it via
 * `fs_core_device_close`. Closing the resulting fs_ext4_fs_t via
 * `fs_ext4_umount` drops the mount's own reference.
 *
 * Returns NULL on failure; use fs_ext4_last_error() / fs_ext4_last_errno()
 * for detail.
 */
fs_ext4_fs_t *fs_ext4_mount_with_fs_core_device_lazy(struct FsCoreDevice *handle);

/*
 * Mount an ext4 filesystem read-write using callback-based I/O.
 * Companion to fs_ext4_mount_rw — same behaviour (replays a dirty journal
 * before returning), but the device is reached through caller-supplied
 * read/write callbacks instead of a path. Suitable for FSKit extensions
 * that own an FSBlockDeviceResource and cannot open /dev/diskN directly.
 *
 * Both `cfg->read` AND `cfg->write` must be non-NULL — otherwise this
 * returns NULL with errno set to EINVAL. `cfg->flush` is optional; pass
 * NULL to make every device flush the driver issues -- including the one
 * fs_ext4_flush() asks for -- a no-op (the caller's host I/O layer is
 * expected to handle stable-storage barriers in that case).
 *
 * Returns NULL on failure; use fs_ext4_last_error() / fs_ext4_last_errno()
 * to inspect the cause.
 */
fs_ext4_fs_t *fs_ext4_mount_rw_with_callbacks(
    const fs_ext4_blockdev_cfg_t *cfg);

/*
 * Mount RW via callbacks WITHOUT performing journal replay automatically.
 * Same semantics as `fs_ext4_mount_rw_with_callbacks` except a dirty
 * journal is recorded but NOT replayed during this call. Use this when
 * the consumer is in a context where its write callback can't service
 * writes yet (e.g. inside FSKit's loadResource, before the kernel opens
 * the writable FD on FSBlockDeviceResource).
 *
 * After mount, call `fs_ext4_replay_journal_if_dirty(fs)` once the
 * consumer's write path is ready. Until journal replay runs, the
 * mounted state may reflect a partially-applied journal — readers
 * see what's on disk, not the post-replay view. Writes through the
 * crate will fail until replay completes.
 *
 * Returns NULL on failure (cfg->read or cfg->write NULL, etc).
 */
fs_ext4_fs_t *fs_ext4_mount_rw_with_callbacks_lazy(
    const fs_ext4_blockdev_cfg_t *cfg);

/*
 * If the volume's journal is dirty, replay it now. Idempotent — safe
 * to call when clean (returns 0, no writes). Returns 0 on success or
 * already-clean, -1 on failure (call fs_ext4_last_error / _last_errno
 * for details). The handle must have been mounted via the `_lazy`
 * variant above; calling on a handle from the eager mount is a no-op
 * and returns 0.
 */
int fs_ext4_replay_journal_if_dirty(fs_ext4_fs_t *fs);

/*
 * Unmount and free all resources.
 */
void fs_ext4_umount(fs_ext4_fs_t *fs);

/*
 * Durability barrier for a live mount. The handle stays mounted and usable.
 *
 * Unmounting is NOT needed to get data out. Every mutating call already
 * writes its changes to their final on-disk location before it returns:
 * a journaled mount commits and checkpoints each transaction immediately,
 * and a mount without a journal writes in ordered stages, with a device
 * flush between each step either way. fs_ext4_flush() adds one more device
 * flush (the `cfg->flush` callback, or fsync for a path mount) and checks
 * that nothing is left in flight: no earlier device write or journal
 * operation failed, and no transaction is left uncheckpointed. On return
 * 0, every mutating call that returned before this one is on stable
 * storage -- as far as the host's flush reaches: with a NULL `cfg->flush`
 * it only means the bytes were handed to `cfg->write`.
 *
 * Safe to call while other calls on the same handle run; a mutation in
 * progress on another thread either completes before the check or starts
 * after it. On a read-only mount there is nothing to write back and it
 * returns 0.
 *
 * Returns 0 on success, -1 on failure with fs_ext4_last_error() /
 * fs_ext4_last_errno() set: EIO when an earlier mutation's device write or
 * journal operation failed -- the mount can no longer vouch for the device
 * and must be unmounted and remounted (which replays or checks) -- or the
 * errno the host's flush surfaced.
 */
int fs_ext4_flush(fs_ext4_fs_t *fs);

/*
 * fs_ext4_flush(), then drop the mount's read caches and re-read the
 * superblock and group descriptors from the device. For a host that
 * changed the device underneath an idle mount and needs the mount to see
 * the device as it now is. The handle stays mounted and usable.
 *
 * NOT concurrent-writer support: the caller must serialise -- no other
 * call on this handle may be in flight, and nothing may write the device
 * while this runs. A change to the volume's identity or geometry is
 * refused (EIO) rather than adopted.
 *
 * A read-only mount of a volume with a dirty journal replays the journal
 * into its cache only; the device still holds the pre-replay bytes, which
 * a read-only mount may not change. There, fs_ext4_fresh_read() returns -1
 * with EIO and keeps the replayed view: dropping the cache would turn
 * committed state back into superseded state. Remount read-write (which
 * replays onto the device) to get a physical readback.
 *
 * Returns 0 on success, -1 on failure with fs_ext4_last_error() /
 * fs_ext4_last_errno() set; every fs_ext4_flush() failure applies.
 */
int fs_ext4_fresh_read(fs_ext4_fs_t *fs);

/* ---- Volume info ---- */

/*
 * Get volume information (name, sizes, free space).
 * Returns 0 on success.
 */
int fs_ext4_get_volume_info(fs_ext4_fs_t *fs,
                                fs_ext4_volume_info_t *info);

/* ---- File attributes ---- */

/*
 * Get attributes for a path (relative to mount root).
 * path should start with "/" e.g. "/etc/passwd"
 * Returns 0 on success.
 */
int fs_ext4_stat(fs_ext4_fs_t *fs, const char *path,
                     fs_ext4_attr_t *attr);

/* ---- Directory listing ---- */

/*
 * Directory iterator — opaque handle.
 */
typedef struct fs_ext4_dir_iter fs_ext4_dir_iter_t;

/*
 * Open a directory for iteration.
 * Returns NULL on failure.
 */
fs_ext4_dir_iter_t *fs_ext4_dir_open(fs_ext4_fs_t *fs,
                                              const char *path);

/*
 * Get the next directory entry.
 * Returns pointer to internal dirent (valid until next call or close).
 * Returns NULL when no more entries.
 */
const fs_ext4_dirent_t *fs_ext4_dir_next(fs_ext4_dir_iter_t *iter);

/*
 * Close directory iterator.
 */
void fs_ext4_dir_close(fs_ext4_dir_iter_t *iter);

/* ---- File reading ---- */

/*
 * Read file contents.
 * Returns bytes read, or -1 on error.
 */
int64_t fs_ext4_read_file(fs_ext4_fs_t *fs, const char *path,
                              void *buf, uint64_t offset, uint64_t length);

/* ---- Symlink ---- */

/*
 * Read symlink target.
 *
 * On success writes the target followed by a NUL terminator into buf and
 * returns the target's length in bytes, not counting the NUL, as Linux
 * readlink(2) does.
 *
 * If bufsize < length + 1 it returns -1 with fs_ext4_last_errno() ==
 * ERANGE, fs_ext4_last_error() naming the size needed, and writes nothing
 * into buf. The target is never silently truncated (unlike readlink(2)).
 *
 * bufsize 0 with a non-NULL buf is only too small: ERANGE as above.
 * NULL fs, path or buf: -1, EINVAL. Any other failure: -1
 * with fs_ext4_last_errno() set (ENOENT, EINVAL when not a symlink, EIO,
 * ENOTSUP for an encrypted target, ...).
 */
int fs_ext4_readlink(fs_ext4_fs_t *fs, const char *path,
                         char *buf, size_t bufsize);

/* ---- Extended attributes ---- */

/*
 * List extended attribute names for a path.
 *
 * Writes NUL-separated fully-qualified names (e.g. "user.color\0user.tag\0")
 * into buf. If buf is NULL or bufsize is 0, no bytes are written but the
 * return value still reports the required total size — use this to probe.
 *
 * Returns: total bytes of output (names + NUL terminators) on success,
 *          -1 on error. If bufsize is less than the required size, writes
 *          as many WHOLE names as fit -- never part of one, which a caller
 *          could not tell from a real name -- and still returns the
 *          required size.
 */
int64_t fs_ext4_listxattr(fs_ext4_fs_t *fs, const char *path,
                              char *buf, size_t bufsize);

/*
 * Get a single extended attribute value by fully-qualified name
 * (e.g. "user.color", "system.posix_acl_access").
 *
 * Writes raw value bytes (no NUL-terminator) into buf. If buf is NULL or
 * bufsize is 0, returns the value size without writing — use this to probe.
 *
 * Returns: value size in bytes on success,
 *          -1 if the name is not present or on error. If bufsize is less
 *          than the value size, writes as much as fits and still returns
 *          the value size.
 */
int64_t fs_ext4_getxattr(fs_ext4_fs_t *fs, const char *path,
                             const char *name, void *buf, size_t bufsize);

/* ---- Error reporting ---- */

/*
 * Get last error message (thread-local).
 * Returns pointer to static/thread-local string.
 */
const char *fs_ext4_last_error(void);

/*
 * Get POSIX errno for the last failed FFI call (thread-local).
 * Returns 0 if the last call succeeded (or no call has been made yet).
 * Codes: ENOENT, EIO, ENOTDIR, EINVAL, ENOTSUP, ESTALE (an inode-addressed
 * call on an inode that no longer names the file) — or any errno surfaced
 * by the underlying I/O layer (e.g. EACCES from the block device).
 * Use this alongside fs_ext4_last_error() to produce an NSError
 * with the correct POSIXErrorDomain code for FSKit.
 */
int fs_ext4_last_errno(void);

/*
 * ----- Write path (Phase 4, in progress) ------------------------------
 *
 * These exports require a read-write mount. Use fs_ext4_mount_rw()
 * for file-backed images; the existing callback mount is read-only.
 * On failure, -1 is returned and fs_ext4_last_error / _last_errno
 * describe the cause.
 */

/* Mount an ext4 filesystem read-write. Returns NULL on failure. A dirty
 * JBD2 journal is replayed before returning so callers see a consistent
 * on-disk state. */
fs_ext4_fs_t *fs_ext4_mount_rw(const char *device_path);

/* Shrink a regular file to `new_size` bytes. Frees the tail extents and
 * updates the inode size + blocks counter. Not yet journaled — safe only
 * on scratch images until the transaction wrapping lands. */
int fs_ext4_truncate(fs_ext4_fs_t *fs, const char *path,
                         uint64_t new_size);

/* Unlink a non-directory file at `path`. Decrements i_links_count; when
 * the count reaches zero the inode's extents are freed, its bitmap bit
 * cleared, and its body zeroed (with i_dtime = now). Refuses directories.
 * Returns 0 on success, -1 on failure. Not yet journaled. */
int fs_ext4_unlink(fs_ext4_fs_t *fs, const char *path);

/* Create a new empty regular file at `path` with the given permission
 * bits (e.g. 0644). Parent must exist and be a directory; the path must
 * not already exist. Returns the allocated inode number on success
 * (> 0), or 0 on failure. Not yet journaled. */
uint32_t fs_ext4_create(fs_ext4_fs_t *fs, const char *path,
                            uint16_t mode);

/* Replace the contents of an existing regular file with `len` bytes from
 * `data`. Frees any previous extents, allocates one contiguous run for
 * the new data, updates size + mtime + ctime. Returns the new size on
 * success, or -1 on failure. Not yet journaled; appends / partial writes
 * are follow-up work. */
int64_t fs_ext4_write_file(fs_ext4_fs_t *fs, const char *path,
                               const void *data, uint64_t len);

/* Positional write — splice `len` bytes from `data` into `path` at byte
 * `offset`. Allocates new physical blocks for any logical blocks not yet
 * mapped (sparse holes / past EOF); existing mapped blocks are read-
 * modify-written so untouched bytes stay intact. The file must already
 * exist (use `fs_ext4_create` first).
 *
 * This is the streaming-write primitive — cost is O(len), not
 * O(filesize). FUSE / WinFsp / FSKit cache-manager dispatches should
 * call this directly instead of merging into a whole-file replace.
 *
 * Returns the new file size on success (>= offset+len), or -1 on
 * failure. Hard cap on `len`: 1 GiB per call. */
int64_t fs_ext4_pwrite(fs_ext4_fs_t *fs, const char *path,
                           const void *data, uint64_t len, uint64_t offset);

/* Rename / move `src` to `dst` within this filesystem. Supports files
 * and directories; cross-parent dir moves fix `..` + parent link counts.
 * Dest must not already exist — for atomic overwrite use
 * `fs_ext4_rename2` with the `FS_EXT4_RENAME_REPLACE` flag. Returns 0
 * on success, -1 on failure. Not yet journaled. */
int fs_ext4_rename(fs_ext4_fs_t *fs, const char *src,
                       const char *dst);

/* Flag bits accepted by `fs_ext4_rename2`. `FS_EXT4_RENAME_REPLACE`
 * enables atomic overwrite of an existing destination, matching POSIX
 * `rename(2)` semantics — required so Windows Explorer "Save As" /
 * drag-drop-onto-existing flows succeed instead of silently failing.
 * Unknown flag bits are rejected with EINVAL so future flag additions
 * stay forward-compatible. */
#define FS_EXT4_RENAME_REPLACE 0x01

/* Rename / move `src` to `dst` within this filesystem with explicit
 * flags. With `FS_EXT4_RENAME_REPLACE` an existing destination is
 * atomically replaced: file→file overwrites and frees the old data,
 * empty-dir → empty-dir overwrites the dropped subdir, crossing the
 * file/directory boundary returns EISDIR / ENOTDIR, and a non-empty
 * destination directory returns ENOTEMPTY. Without the flag, identical
 * to `fs_ext4_rename`. Returns 0 on success, -1 on failure. */
int fs_ext4_rename2(fs_ext4_fs_t *fs, const char *src,
                        const char *dst, int flags);

/* Create a hard link at `dst` pointing to the same inode as `src`.
 * Forbidden on directories. Dest must not already exist. Bumps the
 * shared inode's i_links_count by 1. Returns 0 on success, -1 on
 * failure. Not yet journaled. */
int fs_ext4_link(fs_ext4_fs_t *fs, const char *src,
                     const char *dst);

/* Create a subdirectory at `path` with POSIX permission bits `mode`
 * (typically 0755; low 12 bits used). Parent must exist and be a
 * directory; the path must not already exist. Seeds the new dir with
 * `.` and `..` entries and bumps the parent's i_links_count.
 * Returns the new directory's inode number on success (> 0), or 0 on
 * failure. Not yet journaled. */
uint32_t fs_ext4_mkdir(fs_ext4_fs_t *fs, const char *path,
                           uint16_t mode);

/* Remove an empty directory at `path`. Fails if the target contains
 * entries other than `.` and `..`. Frees its data blocks and inode,
 * removes the entry from the parent, and decrements the parent's
 * i_links_count. Returns 0 on success, -1 on failure. Not yet
 * journaled. */
int fs_ext4_rmdir(fs_ext4_fs_t *fs, const char *path);

/* Change the permission bits on `path`. Only the low 12 bits of `mode`
 * (suid/sgid/sticky + rwx/rwx/rwx) are applied; file-type bits are
 * preserved. Bumps i_ctime. Returns 0 on success, -1 on failure. */
int fs_ext4_chmod(fs_ext4_fs_t *fs, const char *path, uint16_t mode);

/* Change the owner of `path` to (`uid`, `gid`). Passing UINT32_MAX
 * (0xFFFFFFFF) for either parameter leaves that value unchanged
 * (matches Linux chown(2) semantics). Bumps i_ctime. Returns 0 on
 * success, -1 on failure. */
int fs_ext4_chown(fs_ext4_fs_t *fs, const char *path,
                  uint32_t uid, uint32_t gid);

/* Create a special file (FIFO, socket, char device, block device).
 * `mode` must include the file-type bits (S_IFIFO=0x1000, S_IFSOCK=0xC000,
 * S_IFCHR=0x2000, S_IFBLK=0x6000) plus permission bits.
 * `major`/`minor` are device numbers for char/block devices; pass 0
 * for FIFOs and sockets. Returns the new inode number on success, 0 on
 * failure. */
uint32_t fs_ext4_mknod(fs_ext4_fs_t *fs, const char *path,
                       uint16_t mode, uint32_t major, uint32_t minor);

/* Set the i_flags word (FS_IOC_SETFLAGS) on `path`. `flags` is the
 * full new flags value; common flags:
 *   0x00000010  EXT4_IMMUTABLE_FL
 *   0x00000020  EXT4_APPEND_FL
 *   0x00000040  EXT4_NODUMP_FL
 *   0x00000080  EXT4_NOATIME_FL
 * Only the kernel's user-modifiable bits may change (EXT4_FL_USER_MODIFIABLE
 * less EXTENTS, DAX and CASEFOLD); changing any other bit (INDEX, HUGE_FILE,
 * ENCRYPT, VERITY, INLINE_DATA, ...) fails with EINVAL and changes nothing.
 * Bits already set outside that mask may be passed back unchanged.
 * Bumps i_ctime. Returns 0 on success, -1 on failure. */
int fs_ext4_set_flags(fs_ext4_fs_t *fs, const char *path, uint32_t flags);

/* Create a symbolic link at `linkpath` whose target is `target`.
 * POSIX symlink(target, linkpath) semantics. v1: fast-symlink only
 * (target ≤ 60 bytes); longer targets return 0 with EINVAL. Returns
 * the new symlink's inode number on success, 0 on failure. */
uint32_t fs_ext4_symlink(fs_ext4_fs_t *fs, const char *target,
                         const char *linkpath);

/* Set (create or replace) the extended attribute `name` on `path` with
 * `value_len` bytes from `value`. `name` must be fully-qualified
 * (e.g. "user.com.apple.FinderInfo"). v1: in-inode xattrs only;
 * ENOSPC if the in-inode region is too small; external-block spill
 * not implemented. Returns 0 on success, -1 on failure. */
int fs_ext4_setxattr(fs_ext4_fs_t *fs, const char *path,
                     const char *name, const void *value,
                     size_t value_len);

/* Remove the extended attribute `name` from the inode at `path`.
 * `name` must be fully-qualified (carry a known namespace prefix like
 * "user." or "security."). v1: in-inode xattrs only; external-block
 * removal surfaces EINVAL until the slow path lands. ENOENT when the
 * name isn't present. Returns 0 on success, -1 on failure. */
int fs_ext4_removexattr(fs_ext4_fs_t *fs, const char *path,
                        const char *name);

/* Leave a timestamp unchanged: POSIX spells this UTIME_OMIT in the
 * nanoseconds field; fs_ext4_utimens takes it in the seconds field.
 * Not UINT32_MAX any more -- seconds are signed and 64-bit, which makes
 * that value an ordinary date in 2106 rather than a spare bit pattern.
 *
 * A `static const` and not a `#define` so that Swift sees it. Swift's
 * C importer takes simple literal macros only; `#define FS_EXT4_TIME_OMIT
 * INT64_MIN` expands to an expression it declines to import, and the
 * symbol is then simply absent on the Swift side. */
static const int64_t FS_EXT4_TIME_OMIT = INT64_MIN;

/* utimensat(2)'s sentinels, passed in a `*_nsec` field of
 * fs_ext4_utimens. They are SUPPORTED, per field, with utimensat's
 * meaning -- the `*_sec` beside either is ignored:
 *   FS_EXT4_UTIME_NOW   set that timestamp to the current time, read
 *                       from the mount's clock (whole seconds, so its
 *                       nanoseconds are stored as 0);
 *   FS_EXT4_UTIME_OMIT  leave that timestamp unchanged.
 * The values are Linux's UTIME_NOW / UTIME_OMIT, (1 << 30) - 1 and
 * (1 << 30) - 2. Other platforms spell them differently (macOS: -1 and
 * -2), and those values are refused with EINVAL like any other
 * nanosecond count of 1e9 or more -- use these names, not the
 * platform's. `static const` for Swift, as above. */
static const uint32_t FS_EXT4_UTIME_NOW = 0x3fffffffu;
static const uint32_t FS_EXT4_UTIME_OMIT = 0x3ffffffeu;

/* Set the access + modification times on `path`. Each `*_sec` is a
 * POSIX seconds-since-epoch value; passing FS_EXT4_TIME_OMIT leaves that
 * pair unchanged (so `atime_sec == FS_EXT4_TIME_OMIT` touches only
 * mtime, etc). `*_nsec` is sub-second nanoseconds, 0..999999999, only
 * written when the inode's i_extra_isize region can hold them; it also
 * takes FS_EXT4_UTIME_NOW / FS_EXT4_UTIME_OMIT, above. Bumps i_ctime,
 * unless both pairs are omitted, in which case nothing is written.
 *
 * The seconds are int64_t (widened from uint32_t in 0.5.0) because ext4
 * stores a SIGNED 32-bit base extended by two epoch bits in the matching
 * `*_extra` field -- roughly 1901..2446. The old unsigned type could
 * express neither end: a pre-1970 date was unrepresentable, and a
 * post-2038 date was written with the epoch bits left zero and read back
 * 136 years early.
 *
 * Returns 0 on success, -1 on failure. Fails with EINVAL for a time
 * outside that range, for one needing the epoch bits on an inode too
 * small to hold an `*_extra` field (128-byte inodes), or for a `*_nsec`
 * of 1e9 or more that is neither sentinel -- in every case before
 * anything is written. */
int fs_ext4_utimens(fs_ext4_fs_t *fs, const char *path,
                    int64_t atime_sec, uint32_t atime_nsec,
                    int64_t mtime_sec, uint32_t mtime_nsec);

/* ---- Inode-addressed entry points ---- */

/*
 * The path functions above, with the path replaced by what a handle-based
 * host holds instead: an inode number for an item, and a (directory inode,
 * name) pair for a mutation. Each calls the same implementation as its path
 * twin, so the two behave identically -- same attributes, entries, bytes
 * and errors. A path is the wrong key for such a host: a hard link gives
 * one inode several, and renaming a directory changes every one beneath it.
 *
 * GENERATIONS. Every inode argument is followed by a generation: the
 * `generation` field of the fs_ext4_attr_t the caller read the inode number
 * from, or FS_EXT4_GEN_ANY. A call on an inode that has been freed -- or
 * freed and reused for a different file, whose generation differs -- or
 * on a number that never named a file (0, past the inode table, a reserved
 * inode other than the root 2) fails with ESTALE instead of reaching
 * whatever occupies the slot now. With FS_EXT4_GEN_ANY only the reuse case
 * goes undetected. A generation of exactly 0xFFFFFFFF cannot be pinned.
 *
 * NAMES are counted byte buffers (`name`, `name_len`), not C strings, and
 * need not be UTF-8: a name round-trips byte for byte. A name is one
 * directory entry: empty, or containing '/' or NUL, is EINVAL; longer than
 * 255 bytes is ENAMETOOLONG. `.` and `..` are ordinary entries to
 * fs_ext4_lookup_at; fs_ext4_rmdir_at refuses them (EINVAL for `.`,
 * ENOTEMPTY for `..`) and fs_ext4_rename_at refuses them on either side
 * (EINVAL).
 *
 * Return values follow the path twins where those return 0 / -1; the calls
 * that make an inode return 0 / -1 too, and fill an optional `attr`
 * out-parameter (NULL to skip) with the new inode's attributes, from which
 * the caller takes its handle (`attr->inode`, `attr->generation`).
 * Errors are reported through fs_ext4_last_error / fs_ext4_last_errno.
 */
#define FS_EXT4_GEN_ANY 0xFFFFFFFFu

/* Look up `name` in directory `dir_ino`; fill `attr` for the inode it
 * names. 0 on success, -1 on failure (ENOENT, ENOTDIR, ESTALE, ...). */
int fs_ext4_lookup_at(fs_ext4_fs_t *fs, uint32_t dir_ino,
                      uint32_t dir_generation, const char *name,
                      size_t name_len, fs_ext4_attr_t *attr);

/* fs_ext4_stat of inode `ino`. 0 on success, -1 on failure. */
int fs_ext4_stat_ino(fs_ext4_fs_t *fs, uint32_t ino, uint32_t generation,
                     fs_ext4_attr_t *attr);

/* fs_ext4_dir_open of directory `ino`; iterate and close it with
 * fs_ext4_dir_next / fs_ext4_dir_close. NULL on failure. */
fs_ext4_dir_iter_t *fs_ext4_dir_open_ino(fs_ext4_fs_t *fs, uint32_t ino,
                                         uint32_t generation);

/* fs_ext4_read_file of inode `ino`: up to `length` bytes from `offset`.
 * Returns the bytes read, or -1. */
int64_t fs_ext4_pread_ino(fs_ext4_fs_t *fs, uint32_t ino,
                          uint32_t generation, void *buf, uint64_t offset,
                          uint64_t length);

/* fs_ext4_readlink of inode `ino`, under the same contract: the target and
 * a NUL written into `buf`, the target's length returned; -1 with ERANGE,
 * nothing written, when bufsize < length + 1. */
int fs_ext4_readlink_ino(fs_ext4_fs_t *fs, uint32_t ino,
                         uint32_t generation, char *buf, size_t bufsize);

/* fs_ext4_pwrite to inode `ino`. Returns the new size, or -1. */
int64_t fs_ext4_pwrite_ino(fs_ext4_fs_t *fs, uint32_t ino,
                           uint32_t generation, const void *data,
                           uint64_t len, uint64_t offset);

/* fs_ext4_truncate of inode `ino`. 0 on success, -1 on failure. */
int fs_ext4_truncate_ino(fs_ext4_fs_t *fs, uint32_t ino,
                         uint32_t generation, uint64_t new_size);

/* fs_ext4_chmod / fs_ext4_chown / fs_ext4_utimens of inode `ino`.
 * 0 on success, -1 on failure. */
int fs_ext4_chmod_ino(fs_ext4_fs_t *fs, uint32_t ino, uint32_t generation,
                      uint16_t mode);
int fs_ext4_chown_ino(fs_ext4_fs_t *fs, uint32_t ino, uint32_t generation,
                      uint32_t uid, uint32_t gid);
int fs_ext4_utimens_ino(fs_ext4_fs_t *fs, uint32_t ino, uint32_t generation,
                        int64_t atime_sec, uint32_t atime_nsec,
                        int64_t mtime_sec, uint32_t mtime_nsec);

/* fs_ext4_create / fs_ext4_mkdir / fs_ext4_mknod of entry `name` in
 * directory `dir_ino`. 0 on success with `attr` (may be NULL) filled for
 * the new inode, -1 on failure. */
int fs_ext4_create_at(fs_ext4_fs_t *fs, uint32_t dir_ino,
                      uint32_t dir_generation, const char *name,
                      size_t name_len, uint16_t mode, fs_ext4_attr_t *attr);
int fs_ext4_mkdir_at(fs_ext4_fs_t *fs, uint32_t dir_ino,
                     uint32_t dir_generation, const char *name,
                     size_t name_len, uint16_t mode, fs_ext4_attr_t *attr);
int fs_ext4_mknod_at(fs_ext4_fs_t *fs, uint32_t dir_ino,
                     uint32_t dir_generation, const char *name,
                     size_t name_len, uint16_t mode, uint32_t major,
                     uint32_t minor, fs_ext4_attr_t *attr);

/* fs_ext4_symlink: entry `name` in directory `dir_ino` pointing at the
 * NUL-terminated `target` (bytes; need not be UTF-8). 0 on success with
 * `attr` (may be NULL) filled for the new symlink, -1 on failure. */
int fs_ext4_symlink_at(fs_ext4_fs_t *fs, uint32_t dir_ino,
                       uint32_t dir_generation, const char *name,
                       size_t name_len, const char *target,
                       fs_ext4_attr_t *attr);

/* fs_ext4_link: a new entry `name` in directory `dir_ino` for inode
 * `ino`. 0 on success with `attr` (may be NULL) filled for `ino`, -1 on
 * failure (EISDIR for a directory). */
int fs_ext4_link_at(fs_ext4_fs_t *fs, uint32_t ino, uint32_t generation,
                    uint32_t dir_ino, uint32_t dir_generation,
                    const char *name, size_t name_len, fs_ext4_attr_t *attr);

/* fs_ext4_unlink / fs_ext4_rmdir of entry `name` in directory `dir_ino`.
 * 0 on success, -1 on failure. */
int fs_ext4_unlink_at(fs_ext4_fs_t *fs, uint32_t dir_ino,
                      uint32_t dir_generation, const char *name,
                      size_t name_len);
int fs_ext4_rmdir_at(fs_ext4_fs_t *fs, uint32_t dir_ino,
                     uint32_t dir_generation, const char *name,
                     size_t name_len);

/* fs_ext4_rename2 of entry `src_name` in directory `src_dir_ino` to entry
 * `dst_name` in directory `dst_dir_ino`, with the same `flags`
 * (FS_EXT4_RENAME_REPLACE). Renaming an entry to itself succeeds and
 * writes nothing; moving a directory into its own subtree is EINVAL.
 * 0 on success, -1 on failure. */
int fs_ext4_rename_at(fs_ext4_fs_t *fs, uint32_t src_dir_ino,
                      uint32_t src_dir_generation, const char *src_name,
                      size_t src_name_len, uint32_t dst_dir_ino,
                      uint32_t dst_dir_generation, const char *dst_name,
                      size_t dst_name_len, int flags);

/* ---- Volume creation (mkfs) ---- */

/*
 * Format a block device as a fresh ext4 filesystem. The device is reached
 * through the same callback shape used for mounting — `cfg->read`, `cfg->write`
 * and `cfg->size_bytes` must all be set; `cfg->flush` is optional. Works for
 * both `/dev/diskN` (real disk) and disk-image files when the caller wires
 * file-backed callbacks.
 *
 * `label` is an optional NUL-terminated UTF-8 volume name (≤ 16 bytes; longer
 * names are truncated). Pass NULL to leave it blank.
 * `uuid` is either NULL (driver generates a v4 UUID from /dev/urandom) or a
 * pointer to exactly 16 raw bytes the caller has chosen.
 *
 * On-disk layout (v1, single block-group, ≤ 32k blocks): boot sector zero,
 * primary superblock, BGD table, block bitmap, inode bitmap, inode table,
 * root directory data block. Features enabled: FILETYPE, EXTENTS, 64BIT,
 * METADATA_CSUM. No journal — the FS mounts cleanly without one.
 *
 * Returns 0 on success or -errno on failure. Use fs_ext4_last_error /
 * fs_ext4_last_errno for the cause.
 */
int fs_ext4_mkfs(const fs_ext4_blockdev_cfg_t *cfg,
                 const char *label,
                 const uint8_t *uuid);

/*
 * ----- Read-only fsck (filesystem audit) ------------------------------
 *
 * Walks the directory tree, counts directory-entry references to each
 * inode, and reports inconsistencies via callbacks. Never writes — the
 * `read_only` field of `fs_ext4_fsck_options_t` MUST be 1 in this MVP.
 * Repair is explicit future work; a future ABI addition will cover it.
 *
 * Findings are streamed through `on_finding` rather than collected
 * into a returned list, so the host UI can render progress live for
 * very large volumes without buffering the full anomaly set.
 */

typedef enum {
    FS_EXT4_FSCK_PHASE_SUPERBLOCK = 0,
    FS_EXT4_FSCK_PHASE_JOURNAL    = 1,
    FS_EXT4_FSCK_PHASE_DIRECTORY  = 2,
    FS_EXT4_FSCK_PHASE_INODES     = 3,
    FS_EXT4_FSCK_PHASE_FINALIZE   = 4,
} fs_ext4_fsck_phase_t;

/*
 * Progress callback. `phase_name` points to a short ASCII string
 * ("superblock", "journal", "directory", "inodes", "finalize") and is
 * valid only for the duration of the call. `done` and `total` are
 * monotonic within a single phase; `total` may grow during DIRECTORY
 * (queued work is best-estimate). Either or both may be 0 in
 * pathological cases.
 */
typedef void (*fs_ext4_fsck_progress_fn)(void *context,
    fs_ext4_fsck_phase_t phase, const char *phase_name,
    uint64_t done, uint64_t total);

/*
 * Per-finding callback. `kind` is one of: "link_count_low",
 * "link_count_high", "dangling_entry", "wrong_dotdot", "bogus_entry",
 * "duplicate_dir_inode", "block_group_free_count_drift",
 * "superblock_free_count_drift", "dir_block_checksum". `inode` is the
 * most relevant inode (affected inode for link-count cases; child for
 * dangling_entry and bogus_entry; directory for wrong_dotdot and
 * dir_block_checksum, whose detail names the logical block and whether
 * it is an htree index; duplicated dir inode for
 * duplicate_dir_inode; group_index for block_group_free_count_drift;
 * 0 for superblock_free_count_drift — i.e. for the drift kinds the
 * field is overloaded and does not carry an inode number). `detail`
 * is a short, free-form ASCII blob like "stored=1 observed=2" or
 * "parent_ino=2" — for diagnostic display only, not meant to be
 * parsed. Both `kind` and `detail` are valid only for the duration of
 * the call.
 */
typedef void (*fs_ext4_fsck_finding_fn)(void *context,
    const char *kind, uint32_t inode, const char *detail);

typedef struct {
    uint8_t  read_only;            /* 1 = audit only (no writes); 0 = allow repair */
    uint8_t  replay_journal;       /* 1 = invoke replay_journal_if_dirty first */
    uint32_t max_dirs;             /* 0 = unbounded (use u32::MAX internally) */
    uint32_t max_entries_per_dir;  /* 0 = unbounded */
    fs_ext4_fsck_progress_fn on_progress; /* nullable */
    fs_ext4_fsck_finding_fn  on_finding;  /* nullable */
    void *context;
    /*
     * Repair-pass switch. When 1 (and `read_only` is 0) the audit
     * pass commits journaled fixes for the corruption classes it
     * knows how to handle today:
     *   - duplicate dir-entry → same directory inode
     *   - link-count drift (i_links_count != observed dirent count)
     *   - wrong_dotdot (.. claim disagrees with walker's parent)
     *   - bogus_entry (dirent file_type vs. inode mode mismatch)
     *   - dangling_entry (link-count rescue when child is readable)
     *   - block_group_free_count_drift / superblock_free_count_drift
     *   - dir_block_checksum, for a linear block that passes the
     *     structural checks (an htree index stays reported)
     * No repair restamps a directory block whose checksum fails unless
     * it is repairing that block's dir_block_checksum finding.
     * Other anomalies are still detected and reported but not
     * modified. When 0 (or `read_only` == 1) the run is purely
     * diagnostic. Must be 0 or 1; other values are rejected as
     * EINVAL.
     */
    uint8_t  repair;
} fs_ext4_fsck_options_t;

typedef struct {
    uint64_t inodes_visited;
    uint64_t directories_scanned;
    uint64_t entries_scanned;
    /* Authoritative current anomaly count. After a repair pass this
     * is the post-repair RE-SCAN count, not pre-repair minus
     * repaired_count. */
    uint64_t anomalies_found;
    uint8_t  was_dirty;            /* 1 if SB.s_state showed dirty pre-run */
    uint8_t  dirty_cleared;        /* 1 if a repair commit cleared the dirty bit */
    /*
     * Number of anomalies the repair pass committed a fix for. 0 when
     * `repair` was off, when no repairable anomalies were found, or
     * when every found anomaly is in a class fsck doesn't repair yet.
     */
    uint64_t repaired_count;
    /*
     * Anomalies the audit found BEFORE any repair commits. Equal to
     * `anomalies_found` for non-repair runs. After a repair pass:
     * `initial_anomalies_count - repaired_count` is what we EXPECT
     * to remain; `anomalies_found` is what ACTUALLY remains.
     * Mismatches indicate repair-logic bugs.
     */
    uint64_t initial_anomalies_count;
} fs_ext4_fsck_report_t;

/*
 * Run a read-only fsck audit. Writes summary counters to *report and
 * delivers each anomaly through opts->on_finding as it is discovered.
 *
 * Returns 0 on success regardless of how many anomalies were found —
 * anomalies don't fail the call, they're surfaced via on_finding and
 * counted in report->anomalies_found. Returns -1 on hard failure
 * (null args, read_only != 1, journal replay error, I/O error during
 * walk); use fs_ext4_last_error / _last_errno for the cause.
 */
int fs_ext4_fsck_run(fs_ext4_fs_t *fs,
    const fs_ext4_fsck_options_t *opts,
    fs_ext4_fsck_report_t *report);

#ifdef __cplusplus
}
#endif

#endif /* FS_EXT4_H */
