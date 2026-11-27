/* SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause */
/* Copyright 2026 The cladefs Authors. */
/*
 * The filesystem, in Rust (guest/rust/cladefs-linux), as the VFS glue in super.c calls it. Each call
 * may sleep. Errors are negative errnos. The VFS checks what it can first: that names exist or
 * not, that types match, and permissions.
 */
#ifndef _FS_CLADEFS_CLADEFS_H
#define _FS_CLADEFS_CLADEFS_H

#include <linux/types.h>

/* The filesystem on an extent-store device, which its mounted volumes share. */
struct cladefs_filesystem;
/* A mounted volume of one. */
struct cladefs;
struct cladefs_device;

/* The volume a mount naming none mounts, which cladefs's images keep a filesystem's files in. */
#define CLADEFS_DEFAULT_VOLUME "default"

/* An inode's attributes. Its inode number is its object's ID in its volume, its own. */
struct cladefs_attr {
	u64 ino;
	u64 size;
	/* 512-byte blocks the inode's data takes. */
	u64 blocks;
	/* Nanoseconds since the Unix epoch. */
	u64 atime_ns;
	u64 mtime_ns;
	u64 ctime_ns;
	/* With the inode's attribute flags above the kernel's bits (CLADEFS_MODE_*). */
	u32 mode;
	u32 nlink;
	u32 uid;
	u32 gid;
	/* A device file's device. */
	u64 rdev;
};

/* Which of `struct cladefs_setattr`'s attributes to set. */
#define CLADEFS_SET_MODE (1 << 0)
#define CLADEFS_SET_UID (1 << 1)
#define CLADEFS_SET_GID (1 << 2)
#define CLADEFS_SET_SIZE (1 << 3)
#define CLADEFS_SET_ATIME (1 << 4)
#define CLADEFS_SET_MTIME (1 << 5)
/* Otherwise a change's ctime is the time of the change. */
#define CLADEFS_SET_CTIME (1 << 6)
/* A truncate leaves the last block as it is: a DAX window zeroes it, which maps that block. */
#define CLADEFS_SET_KEEP_TAIL (1 << 7)

/*
 * cladefs: the attribute flags (chattr) an inode's mode keeps above the kernel's 16 bits: Fxfs's
 * objects have no record of them of their own, and its mode is 32 bits.
 */
#define CLADEFS_MODE_IMMUTABLE (1u << 16)
#define CLADEFS_MODE_APPEND (1u << 17)
#define CLADEFS_MODE_NOATIME (1u << 18)
#define CLADEFS_MODE_NODUMP (1u << 19)

/* Attributes to set: those `valid` says. */
struct cladefs_setattr {
	u32 valid;
	u32 mode;
	u32 uid;
	u32 gid;
	u64 size;
	u64 atime_ns;
	u64 mtime_ns;
	u64 ctime_ns;
};

/* What cladefs_unlink and cladefs_rename removed or replaced. */
#define CLADEFS_REPLACED_NONE 0
/* A file or symlink, which other names may still link. */
#define CLADEFS_REPLACED_OBJECT 1
/* A directory, which no name links any more, until the kernel evicts it. */
#define CLADEFS_REPLACED_DIRECTORY 2

/* The filesystem's usage. */
struct cladefs_statfs {
	u64 block_size;
	u64 blocks;
	u64 free_blocks;
	u64 files;
};

/* Readies Rust's runtime, once, before any other call. */
void cladefs_init(void);

/*
 * Opens Fxfs on the extent-store device tagged with the `len` bytes of `source`, formatting it if
 * its store holds nothing yet, with an empty default volume: 0, or a -errno (-ENOENT if there is no
 * such device). Fxfs sizes what it keeps in memory by the `memory` bytes the kernel has. cladefs:
 * read-only if `read_only`.
 */
int cladefs_open(const char *source, size_t len, u64 memory, bool read_only,
	      struct cladefs_filesystem **filesystem);
/* Closes the filesystem, writing back what is dirty, once no volume of it is mounted. */
void cladefs_close(struct cladefs_filesystem *filesystem);
/*
 * Mounts the volume named with the `len` bytes of `volume`, and fills in its root directory's
 * attributes: 0, or a -errno (-ENOENT if there is no such volume). cladefs: read-only if `read_only`,
 * or if the filesystem is.
 */
int cladefs_mount(struct cladefs_filesystem *filesystem, const char *volume, size_t len, bool read_only,
	       struct cladefs **fs, struct cladefs_attr *root);
/* cladefs: whether the filesystem is mounted read-only. */
bool cladefs_read_only(struct cladefs *fs);
/* Unmounts the volume, applying what it queued. The filesystem stays open. */
void cladefs_unmount(struct cladefs *fs);
/* Makes what the filesystem holds durable: sync, syncfs. */
int cladefs_sync(struct cladefs *fs);
/*
 * cladefs: freezes the filesystem, each of whose volumes the VFS has synced, its writers stopped:
 * makes it durable, holds its background work, and closes the device to writes until it thaws,
 * nothing open on it; -EBUSY if writes kept coming. Thawing, it takes the device's cell, a fork's
 * of its own in a VM restored from a snapshot taken frozen, and writes again (TASK-39.1).
 */
int cladefs_freeze(struct cladefs_filesystem *filesystem);
/*
 * A thaw that fails leaves the filesystem frozen, and the next goes on from the step it stopped at
 * (TASK-200). cladefs_is_frozen reports the filesystem's single authoritative freeze state, which
 * stays true until every thaw step succeeds. cladefs_set_fail_thaw_at, fault injection: the next
 * thaw fails before its step `step`, 1 telling the device, 2 rotating the journal's span, and
 * cladefs_fail_thaw_at is 0 again; 0, none.
 */
int cladefs_thaw(struct cladefs_filesystem *filesystem);
bool cladefs_is_frozen(struct cladefs_filesystem *filesystem);
void cladefs_set_fail_thaw_at(u32 step);
u32 cladefs_fail_thaw_at(void);
/*
 * Makes what the filesystem holds survive the guest's crash, before what comes after: fsync, which
 * need not wait for the host's disk (doc-2's requirement 2).
 */
int cladefs_fsync(struct cladefs *fs);
/*
 * Takes the size the host resized the store to, as the device's configuration says, while mounted:
 * grows the filesystem, or, asked to shrink, shrinks it if it holds no more, and answers.
 */
int cladefs_resize(struct cladefs_filesystem *filesystem);
int cladefs_statfs(struct cladefs *fs, struct cladefs_statfs *statfs);

/* Frees the blocks of file `ino` within the `len` bytes at `offset`, block-aligned: a hole. */
int cladefs_punch(struct cladefs *fs, u64 ino, u64 offset, u64 len);
/*
 * The attributes of inode `ino`, for a file handle: -ENOENT if no name links it and nothing has it
 * open, or it is gone.
 */
int cladefs_getattr(struct cladefs *fs, u64 ino, struct cladefs_attr *attr);
/* cladefs: as cladefs_getattr, but of an object no name links too, which something holds open. */
int cladefs_attr_of_open(struct cladefs *fs, u64 ino, struct cladefs_attr *attr);
/* cladefs: how far the journal is written: every commit moves it, all of it applied. */
u64 cladefs_journal_offset(struct cladefs *fs);
/* Writes back what the kernel keeps of inode `ino`: the attributes `set` says. */
int cladefs_write_inode(struct cladefs *fs, u64 ino, const struct cladefs_setattr *set);

/* The attributes of `name` in directory `dir`: -ENOENT if there is none. */
int cladefs_lookup(struct cladefs *fs, u64 dir, const char *name, size_t len, struct cladefs_attr *attr);

/*
 * Calls `emit` for the entries of directory `dir`, in order, until `emit` returns false: those
 * after the entry named with the `after_len` bytes of `after`, which may have been removed since,
 * or without `after`, from the `index`th on.
 */
typedef bool (*cladefs_emit_t)(void *ctx, const char *name, size_t len, u64 ino, u32 mode);
int cladefs_readdir(struct cladefs *fs, u64 dir, u64 index, const char *after, size_t after_len,
		 cladefs_emit_t emit, void *ctx);

/*
 * Each change to a directory's names fills in the attributes of the inodes it changed, as it
 * left them: the directories', and those of the inodes it linked.
 */

/* cladefs: an extended attribute a new inode is made with (cladefs_create): its full name, its value. */
struct cladefs_xattr {
	const char *name;
	size_t name_len;
	const void *value;
	size_t value_len;
};

/*
 * Creates `name` in directory `dir` as `mode` says: a directory, a regular file, or a special
 * file (FIFO, socket or device `rdev`), owned by `uid` and `gid`. cladefs: with the `nr_xattrs`
 * extended attributes `xattrs`, the ACLs it inherits, in the same transaction (TASK-79.29).
 */
int cladefs_create(struct cladefs *fs, u64 dir, const char *name, size_t len, u32 mode, u64 rdev,
		u32 uid, u32 gid, const struct cladefs_xattr *xattrs, size_t nr_xattrs,
		struct cladefs_attr *attr, struct cladefs_attr *dir_attr);
/* The longest target a symlink has: PATH_MAX - 1, as Fxfs's MAX_LONG_SYMLINK. */
#define CLADEFS_SYMLINK_MAX 4095

/*
 * Creates the symlink `name` in directory `dir`, to the `target_len` bytes of `target`, owned by
 * `uid` and `gid`.
 */
int cladefs_symlink(struct cladefs *fs, u64 dir, const char *name, size_t len, const char *target,
		 size_t target_len, u32 uid, u32 gid, struct cladefs_attr *attr,
		 struct cladefs_attr *dir_attr);
/* Copies up to `len` bytes of symlink `ino`'s target to `buf`, returning its length. */
ssize_t cladefs_readlink(struct cladefs *fs, u64 ino, char *buf, size_t len);
/*
 * cladefs: makes file `dst`'s `len` bytes at `dst_offset` share the extents holding file `src`'s at
 * `offset` (a reflink), copying nothing, and `dst` `size` bytes long if it is shorter: its
 * attributes in `attr`. The offsets and `len` are block aligned. -EOPNOTSUPP for extents written
 * in place, which can't be shared.
 */
int cladefs_clone_range(struct cladefs *fs, u64 src, u64 offset, u64 len, u64 dst, u64 dst_offset,
		     u64 size, struct cladefs_attr *attr);
/*
 * Links file or symlink `ino` into directory `dir` as `name`: if `unnamed`, an unnamed file
 * (cladefs_tmpfile), which leaves the graveyard.
 */
int cladefs_link(struct cladefs *fs, u64 ino, u64 dir, const char *name, size_t len, bool unnamed,
	      struct cladefs_attr *attr, struct cladefs_attr *dir_attr);
/*
 * Creates an unnamed regular file, as `mode` says, owned by `uid` and `gid`, for directory `dir`
 * (O_TMPFILE): in the graveyard, which frees it once evicted unless a link names it first.
 */
int cladefs_tmpfile(struct cladefs *fs, u64 dir, u32 mode, u32 uid, u32 gid, struct cladefs_attr *attr);
/*
 * The kernel's locks keep which inode a name links; removing or replacing one, it says which:
 * Fxfs's lookups needn't find out first.
 */

/*
 * Removes `name`, which links inode `ino`, no directory, from directory `dir` later, in a batch:
 * before anything reads what it changes, and before syncs. 0, or a -errno.
 */
int cladefs_unlink_later(struct cladefs *fs, u64 dir, const char *name, size_t len, u64 ino);
/* Removes `name`, which links inode `ino`, from directory `dir`: CLADEFS_REPLACED_*, or a -errno. */
int cladefs_unlink(struct cladefs *fs, u64 dir, const char *name, size_t len, u64 ino,
		struct cladefs_attr *dir_attr);
/*
 * Moves `name`, which links inode `ino`, in `dir` to `new_name` in `new_dir`, replacing inode
 * `new_ino` there, 0 if none: CLADEFS_REPLACED_*, or a -errno. `attr` is the moved inode's.
 */
int cladefs_rename(struct cladefs *fs, u64 dir, const char *name, size_t len, u64 ino, u64 new_dir,
		const char *new_name, size_t new_len, u64 new_ino, struct cladefs_attr *attr,
		struct cladefs_attr *dir_attr, struct cladefs_attr *new_dir_attr);
/*
 * Exchanges `name` in `dir` and `new_name` in `new_dir`, which both exist (RENAME_EXCHANGE):
 * 0, or a -errno. `attr` is the inode `name` linked, and `new_attr` the one `new_name` did.
 */
int cladefs_exchange(struct cladefs *fs, u64 dir, const char *name, size_t len, u64 new_dir,
		  const char *new_name, size_t new_len, struct cladefs_attr *attr,
		  struct cladefs_attr *new_attr, struct cladefs_attr *dir_attr,
		  struct cladefs_attr *new_dir_attr);
/*
 * cladefs: how many records Fxfs caches (its objects', names' and data's), and frees up to `count` of
 * them, those used least recently first, returning how many it freed: the superblock's shrinker's.
 */
u64 cladefs_cached_objects(struct cladefs *fs);
u64 cladefs_free_cached_objects(struct cladefs *fs, u64 count);
/*
 * cladefs: writes what the filesystem waited on, and its device's requests took, as lines of a name
 * and a number, into the `len` bytes at `buf`, as much as fits; returns how long it is (TASK-91.7).
 */
size_t cladefs_stats(struct cladefs_filesystem *filesystem, char *buf, size_t len);

/*
 * Closes inode `ino`, which the kernel evicts, and frees it if `unlinked`: if no name links it, a
 * file's or a directory's, removed.
 */
void cladefs_evict(struct cladefs *fs, u64 ino, bool unlinked);

/* Reads up to `len` bytes of file `ino` at `offset` into `buf`, returning how many it read. */
ssize_t cladefs_read(struct cladefs *fs, u64 ino, u64 offset, void *buf, size_t len);
/* One of cladefs_read_into's buffers: room for `len` bytes at `buf`. */
struct cladefs_read_part {
	void *buf;
	size_t len;
};

/*
 * cladefs: reads file `ino` from `offset` into the `count` buffers `parts`, one after another, as
 * cladefs_read into one, but from the device's transfer buffer straight into them: readahead's folios
 * (TASK-92.9). Returns how many bytes it read.
 */
ssize_t cladefs_read_into(struct cladefs *fs, u64 ino, u64 offset, const struct cladefs_read_part *parts,
		       size_t count);
/*
 * Writes the `len` bytes of `buf` to file `ino` at `offset`, growing the file if it ends before
 * them. The file's times are the kernel's to set (cladefs_write_inode).
 */
int cladefs_write(struct cladefs *fs, u64 ino, u64 offset, const void *buf, size_t len);

/* One of cladefs_write_many's writes: the `len` bytes at `data`, to `offset` in the file. */
struct cladefs_write_part {
	u64 offset;
	const void *data;
	size_t len;
};

/*
 * Writes each of the `count` parts of `parts` to file `ino`, as cladefs_write writes one, in as few
 * transactions as hold them, each with one write to the device: the page cache's writeback of a
 * file's folios, contiguous or not, in any order. No two share a block. cladefs: `dirty` bytes of
 * the file's dirty pages, which they write, have room reserved (cladefs_reserve_dirty), which pays
 * for them: a filesystem filled since doesn't fail them (TASK-80).
 */
int cladefs_write_many(struct cladefs *fs, u64 ino, const struct cladefs_write_part *parts, size_t count,
		    u64 dirty);
/*
 * cladefs: reserves room for `bytes` more of file `ino`'s data, whole pages, which the page cache
 * dirties: 0, or -ENOSPC if the filesystem hasn't room for them, and for writing them back
 * (TASK-80).
 */
int cladefs_reserve_dirty(struct cladefs *fs, u64 ino, u64 bytes);
/* cladefs: as cladefs_reserve_dirty, for pages dirtied whether the filesystem has room or not. */
void cladefs_reserve_dirty_what_fits(struct cladefs *fs, u64 ino, u64 bytes);
/* cladefs: releases the room of `bytes` of file `ino`'s dirty pages, dropped unwritten. */
void cladefs_release_dirty(struct cladefs *fs, u64 ino, u64 bytes);
/*
 * cladefs: fault injection (TASK-164.2): the next `count` of cladefs_write_many's transactions fail with
 * EIO once their blocks are written, before they commit, leaving the file as a crash between the
 * two would: its old blocks, the new ones named by no record.
 */
void cladefs_set_fail_commits(u64 count);

/* The extent-store device the filesystem is on (device.h). */
struct cladefs_device *cladefs_dax_device(struct cladefs_filesystem *filesystem);

/*
 * Whether the DAX window may map a piece of a file: Fxfs's ExtentAccess, by these numbers. The
 * window maps sealed extents alone (doc-13).
 */
enum cladefs_dax_access {
	/*
	 * Its extent isn't known to be open: a seal came since it was minted, which sealed it, or
	 * found nothing of it on the device yet. Read-only for good, anywhere in the window, whatever
	 * seals meanwhile.
	 */
	CLADEFS_DAX_SEALED,
	/* Its extent is open still, which a seal must come for first; as is any other number. */
	CLADEFS_DAX_OPEN,
};

/*
 * A piece of a file's data, which the DAX window maps (dax.c): `len` bytes at `offset` in the file,
 * at `extent_offset` in the extent {cell, sequence}, as `access`, an enum cladefs_dax_access, says.
 * Offsets and lengths are multiples of the block, the page.
 */
struct cladefs_dax_extent {
	u64 offset;
	u64 len;
	u64 cell;
	u64 sequence;
	u64 extent_offset;
	/* enum cladefs_dax_access. */
	u32 access;
	u32 reserved;
	/*
	 * How far the extent reaches, as allocated, or may yet if it is the staging extent, which
	 * small files share: 0 if unknown.
	 */
	u64 extent_size;
};

/*
 * The pieces of file `ino`'s data within `len` bytes at `offset`, in order, at most `max`, and one
 * per block at most; holes have none. Returns how many, or a -errno.
 */
ssize_t cladefs_dax_extents(struct cladefs *fs, u64 ino, u64 offset, u64 len,
			 struct cladefs_dax_extent *out, size_t max);
/* The first max pieces in the interval. A short batch exhausts the lookup. */
ssize_t cladefs_extent_batch(struct cladefs *fs, u64 ino, u64 offset, u64 len,
			     struct cladefs_dax_extent *out, size_t max);
/*
 * Extended attributes of inode `ino`, by their full names (`user.x`): getxattr(2)'s and
 * listxattr(2)'s semantics, ssize_t results as theirs (the length, or with `size` 0 the length
 * needed, or -ERANGE), and setxattr(2)'s flags. A change is made at once, in a transaction of its
 * own: -ENOSPC if the filesystem has no room for it (TASK-199).
 */
ssize_t cladefs_getxattr(struct cladefs *fs, u64 ino, const char *name, size_t len, void *buf,
		      size_t size);
int cladefs_setxattr(struct cladefs *fs, u64 ino, const char *name, size_t len, const void *value,
		  size_t value_len, int flags);
ssize_t cladefs_listxattr(struct cladefs *fs, u64 ino, char *buf, size_t size);
int cladefs_removexattr(struct cladefs *fs, u64 ino, const char *name, size_t len);

#endif
