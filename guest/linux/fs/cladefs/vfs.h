/* SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause */
/* Copyright 2026 The cladefs Authors. */
/*
 * What Fxfs's VFS glue (super.c), its changes of files' data (data.c) and its DAX (dax.c) share:
 * the superblock's and inodes' state, and calls into Rust.
 */
#ifndef _FS_CLADEFS_VFS_H
#define _FS_CLADEFS_VFS_H

#include <linux/fs.h>
#include <linux/gfp.h>
#include <linux/list.h>
#include <linux/mutex.h>
#include <linux/rwsem.h>
#include <linux/sched/mm.h>
#include <linux/workqueue.h>
#include <linux/xarray.h>

#include "cladefs.h"

/*
 * Evaluates `call`, a call into Rust, in a NOFS allocation scope: Rust's allocator may reclaim
 * memory, which must not recurse into filesystems.
 */
#define cladefs_nofs(call)                                        \
	({                                                     \
		unsigned int __nofs = memalloc_nofs_save();    \
		typeof(call) __ret = (call);                   \
		memalloc_nofs_restore(__nofs);                 \
		__ret;                                         \
	})

static inline void cladefs_nofs_leave(unsigned int *was)
{
	memalloc_nofs_restore(*was);
}

/*
 * cladefs: a NOFS allocation scope from here to the end of the enclosing block, which no return
 * skips, so that scopes close in the order they opened, as memalloc_nofs_restore needs: for what
 * holds a lock reclaim may take (cladefs_nofs_held).
 */
#define cladefs_nofs_scope()                                                              \
	unsigned int __cladefs_nofs __attribute__((__cleanup__(cladefs_nofs_leave))) = \
		memalloc_nofs_save()

/*
 * cladefs: a lock reclaim from the filesystem may take, a DAX file's writeback lock and semaphore
 * (dax.c), taken and let go of in a NOFS scope alone, which these check at each: so that no
 * allocation under it, of whatever code, reclaims from the filesystem and waits on it.
 */
static inline void cladefs_nofs_held(void)
{
	WARN_ON_ONCE(current_gfp_context(GFP_KERNEL) & __GFP_FS);
}

static inline void cladefs_nofs_down_read(struct rw_semaphore *sem)
{
	cladefs_nofs_held();
	down_read(sem);
}

static inline void cladefs_nofs_up_read(struct rw_semaphore *sem)
{
	cladefs_nofs_held();
	up_read(sem);
}

static inline void cladefs_nofs_down_write(struct rw_semaphore *sem)
{
	cladefs_nofs_held();
	down_write(sem);
}

static inline int cladefs_nofs_down_write_trylock(struct rw_semaphore *sem)
{
	cladefs_nofs_held();
	return down_write_trylock(sem);
}

static inline void cladefs_nofs_up_write(struct rw_semaphore *sem)
{
	cladefs_nofs_held();
	up_write(sem);
}

static inline void cladefs_nofs_mutex_lock(struct mutex *lock)
{
	cladefs_nofs_held();
	mutex_lock(lock);
}

static inline void cladefs_nofs_mutex_unlock(struct mutex *lock)
{
	cladefs_nofs_held();
	mutex_unlock(lock);
}

struct virtio_extent;
struct cladefs_dax;
struct cladefs_dax_volume;

/*
 * The filesystem on a device, which the superblocks of its mounted volumes share: open while one
 * is mounted, or mounting, and kept to be opened again.
 */
struct cladefs_vfs_filesystem {
	/* On the filesystems known, one for each device a filesystem was mounted on. */
	struct list_head list;
	struct virtio_extent *vext;
	/* Guards the rest, and opens and closes the filesystem. */
	struct mutex lock;
	struct cladefs_filesystem *filesystem;
	/* Its DAX, if it has one: one window, which all its volumes share. */
	struct cladefs_dax *dax;
	/* cladefs: /proc/fs/cladefs/<its device's tag>, what it waited on (cladefs_vfs_stats_show). */
	struct proc_dir_entry *stats;
	/* cladefs: readahead's reads, off their readers (cladefs_vfs_readahead). */
	struct workqueue_struct *readahead;
	/* cladefs: takes the size the host resizes the store to (cladefs_resize), off the interrupt. */
	struct work_struct resize;
	/*
	 * cladefs: the writes that found the filesystem full and asked for room (cladefs_vfs_make_room),
	 * and the syncs that made it, begun and done, which `room_lock` serializes (TASK-149.25).
	 */
	atomic64_t room_asked;
	struct mutex room_lock;
	u64 room_syncs_begun;
	u64 room_syncs_done;
	/*
	 * Its superblocks, and how many of those are frozen (`struct cladefs_vfs_info`'s `frozen`),
	 * read-only ones too (cladefs_vfs_settle_frozen). Whether the filesystem itself is frozen
	 * lives in Rust (`cladefs_is_frozen`), the single authoritative owner across VFS, DAX, Fxfs
	 * and the device.
	 */
	unsigned int volumes;
	unsigned int volumes_frozen;
};

/*
 * A superblock's: its volume's, and the filesystem and DAX it shares. A volume has its own inode
 * numbers, st_dev and `EXDEV`, and only the filesystem's journal, device and window in common.
 */
struct cladefs_vfs_info {
	struct virtio_extent *vext;
	/* The volume's name, as it is mounted, and what mounts it. */
	char *volume;
	struct cladefs_vfs_filesystem *filesystem;
	struct cladefs *fs;
	struct cladefs_dax *dax;
	struct cladefs_dax_volume *dax_volume;
	/*
	 * cladefs: whether its filesystem counts it frozen (`volumes_frozen`), which its filesystem's
	 * lock guards: a writable superblock as the VFS freezes it (freeze_fs) and thaws it
	 * (unfreeze_fs); a read-only one, which the VFS freezes without a call to its filesystem, as
	 * cladefs_vfs_freeze_super and cladefs_vfs_thaw_super find it frozen or not (TASK-202).
	 */
	bool frozen;
};

/*
 * cladefs: CLADEFS_IOC_CACHED_OBJECTS, on any directory: how many records Fxfs caches, which the
 * superblock's shrinker frees under memory pressure (cladefs_cached_objects).
 */
#define CLADEFS_IOC_CACHED_OBJECTS _IOR('x', 0x44, __u64)

/* 0x45 to 0x47 were the ioctls of shared volumes (removed, board 14527). */

static inline struct cladefs_vfs_info *cladefs_vfs_info(struct super_block *sb)
{
	return sb->s_fs_info;
}

static inline struct cladefs *cladefs_vfs_fs(struct super_block *sb)
{
	return cladefs_vfs_info(sb)->fs;
}

/* llseek(2), with SEEK_DATA and SEEK_HOLE, for regular files, DAX or not (super.c). */
loff_t cladefs_vfs_llseek(struct file *file, loff_t offset, int whence);
/*
 * cladefs: makes room in a filesystem a write found full, by writing back what is dirty, which frees
 * what was reserved for it beyond what its writeback takes (super.c): a DAX write's too (dax.c).
 */
void cladefs_vfs_make_room(struct super_block *sb);
/* fsync(2), for files of any kind, DAX or not (super.c). */
int cladefs_vfs_fsync(struct file *file, loff_t start, loff_t end, int datasync);
/*
 * Writes what the kernel keeps of `inode` to Fxfs: its mode, owner and times, and with
 * CLADEFS_SET_SIZE in `valid`, its size (super.c).
 */
int cladefs_vfs_write_attr(struct inode *inode, u32 valid);

/*
 * cladefs: what holds a regular file's data, the page cache (super.c) or the DAX window (dax.c): what
 * each does around data.c's changes of the file as a whole, which are the same for both, and which
 * guest/sim builds as they are (TASK-160.2). An inode's, as its i_fop and a_ops are, set with them.
 * With the inode's lock held for writing.
 */
struct cladefs_data_ops {
	/*
	 * Before and after Fxfs takes `inode`'s new size, `size`: the first readies the pages past
	 * the old end or the new one; the second, if any, ends what the first began, with the size
	 * the inode has then, which is the old one again if Fxfs couldn't grow the file.
	 */
	int (*resize_begin)(struct inode *inode, loff_t size);
	void (*resize_end)(struct inode *inode, loff_t size);
	/*
	 * Whether a cut leaves Fxfs the last block as it is (CLADEFS_SET_KEEP_TAIL): its bytes past
	 * the end are zeroed already, by resize_begin.
	 */
	bool keeps_tail;
	/*
	 * A hole punched from `start` to `end`, with the invalidate_lock held for writing: the
	 * first readies the range; `zero` zeroes `len` bytes at `pos`, within a page, each of its
	 * partial pages; and the last, if any, lets go of its whole pages from `start` to `end`,
	 * before Fxfs frees their blocks.
	 */
	int (*punch_begin)(struct inode *inode, loff_t start, loff_t end);
	int (*zero)(struct inode *inode, loff_t pos, loff_t len);
	int (*punch_drop)(struct inode *inode, loff_t start, loff_t end);
	/*
	 * A reflink of `len` bytes at `pos`, to or from the inode, which grows to `size` bytes if
	 * that is longer, with its invalidate_lock held for writing: the first readies them, before
	 * Fxfs shares the blocks; the second, if any, of a destination's, after.
	 */
	int (*remap_begin)(struct inode *inode, loff_t pos, loff_t len, loff_t size);
	int (*remapped)(struct inode *inode, loff_t pos, loff_t len);
};

/*
 * data.c: changes of a regular file as a whole, the same whatever holds its data (cladefs_data_ops).
 * cladefs_data_resize makes `inode` `size` bytes long, with the attributes `iattr` sets, if any,
 * written back with it; the others are fallocate(2) and reflinks (FICLONE, FICLONERANGE,
 * copy_file_range), as file operations.
 */
int cladefs_data_resize(struct inode *inode, loff_t size, struct mnt_idmap *idmap,
		     struct iattr *iattr);
long cladefs_data_fallocate(struct file *file, int mode, loff_t offset, loff_t len);
loff_t cladefs_data_remap_file_range(struct file *file_in, loff_t pos_in, struct file *file_out,
				  loff_t pos_out, loff_t len, unsigned int remap_flags);

struct cladefs_dax_file;
struct cladefs_vfs_dir_listing;

/* A regular file's DAX state (dax.c). */
struct cladefs_dax_inode {
	/* The file's ranges of the window, which outlive the inode, if it is a DAX inode. */
	struct cladefs_dax_file *file;
};

struct cladefs_vfs_inode {
	/* cladefs: what holds its data, if it is a regular file. */
	const struct cladefs_data_ops *data;
	struct cladefs_dax_inode dax;
	/* cladefs: a directory's entries, kept while it is cached, under i_lock (super.c). */
	struct cladefs_vfs_dir_listing *listing;
	/* Its attribute flags (FS_*_FL), which Fxfs keeps in its mode (CLADEFS_MODE_*). */
	struct mutex attr_lock; /* Serializes attribute writeback with flag publication. */
	u32 fsflags;
	/* cladefs: how far the journal was written as i_blocks was read (cladefs_vfs_getattr). */
	u64 blocks_as_of;
	struct inode vfs;
};

static inline struct cladefs_vfs_inode *cladefs_vfs_inode(struct inode *inode)
{
	return container_of(inode, struct cladefs_vfs_inode, vfs);
}

/* dax.c: files' data mapped from the extent-store device's DAX window. */
#if IS_ENABLED(CONFIG_FS_DAX)

/*
 * The DAX of `filesystem`, if its device's window can map files' data; NULL if it can't, or an
 * ERR_PTR.
 */
struct cladefs_dax *cladefs_dax_create(struct cladefs_filesystem *filesystem);
/*
 * `dax`'s share for the volume `fs`, mounted: its files, by their inode numbers; or an ERR_PTR.
 * cladefs_dax_remove_volume gives it back, before the volume unmounts, with others staying; otherwise
 * cladefs_dax_destroy frees it.
 */
struct cladefs_dax_volume *cladefs_dax_add_volume(struct cladefs_dax *dax, struct cladefs *fs);
/*
 * Lets go of what `dax`'s window keeps of `volume`, its inodes evicted, its data synced, and frees
 * it, with the budget's worker stopped (cladefs_dax_stop): the window goes on for the other volumes.
 */
void cladefs_dax_remove_volume(struct cladefs_dax *dax, struct cladefs_dax_volume *volume);
/* Writes back every DAX file's dirty pages, before a sync: 0, or the first error. */
int cladefs_dax_settle(struct cladefs_dax *dax);
/*
 * cladefs: stops the budget's worker (dax.c), and waits for it, before a volume's inodes are
 * evicted. Stops are counted: cladefs_dax_start balances one, and the worker runs again once
 * none holds it, a freeze's (cladefs_dax_quiesce) included (board 12453).
 */
void cladefs_dax_stop(struct cladefs_dax *dax);
void cladefs_dax_start(struct cladefs_dax *dax);
/*
 * cladefs: the filesystem is frozen, its commits uploaded, nothing dirty: every private page is
 * rebound to its extent's, for a snapshot to take the window (doc-13). cladefs_dax_thaw ends it.
 */
void cladefs_dax_frozen(struct cladefs_dax *dax);
/*
 * Stops `dax`'s background work, once nothing writes, before the filesystem freezes;
 * cladefs_dax_thaw balances it (cladefs_dax_stop, cladefs_dax_start).
 */
void cladefs_dax_quiesce(struct cladefs_dax *dax);
void cladefs_dax_thaw(struct cladefs_dax *dax);
/* Frees `dax` once the filesystem is unmounted. */
void cladefs_dax_destroy(struct cladefs_dax *dax);
/* cladefs: shows how the window is used, as lines of a name and a number (TASK-91.7). */
void cladefs_dax_show_stats(struct cladefs_dax *dax, struct seq_file *m);
void cladefs_dax_inode_init(struct cladefs_dax_inode *di);
/*
 * Makes the regular file `inode` a DAX inode, as it is read in, or `created`, and returns whether
 * it did. cladefs: one created in this mount isn't, nor one read in again before the barrier sealing
 * its page-cache writes (doc-13, TASK-164.3).
 */
bool cladefs_dax_set_inode(struct inode *inode, bool created);
/*
 * Detaches `inode`, which the kernel evicts, from its file's window mappings, which stay for the
 * next inode read in for it, its dirty pages written back first: unless no name links it, when
 * they go.
 */
void cladefs_dax_evict(struct inode *inode);
/*
 * cladefs: the page-cache file `inode`'s writeback wrote its data to extents, open until the barrier
 * after: read in again before it, it is a page-cache inode still (cladefs_dax_set_inode, doc-13).
 */
void cladefs_dax_written(struct inode *inode);
/*
 * cladefs: writes back a DAX file's dirty pages, which Fxfs takes its size from, for fsync, seeks,
 * fiemap and the like (doc-13): 0, or a -errno. Nothing for another file.
 */
int cladefs_dax_write_back(struct inode *inode);
#else
static inline struct cladefs_dax *cladefs_dax_create(struct cladefs_filesystem *filesystem)
{
	return NULL;
}
static inline struct cladefs_dax_volume *cladefs_dax_add_volume(struct cladefs_dax *dax, struct cladefs *fs)
{
	return NULL;
}
static inline void cladefs_dax_remove_volume(struct cladefs_dax *dax, struct cladefs_dax_volume *volume) {}
static inline int cladefs_dax_settle(struct cladefs_dax *dax)
{
	return 0;
}
static inline void cladefs_dax_stop(struct cladefs_dax *dax) {}
static inline void cladefs_dax_start(struct cladefs_dax *dax) {}
static inline void cladefs_dax_frozen(struct cladefs_dax *dax) {}
static inline void cladefs_dax_quiesce(struct cladefs_dax *dax) {}
static inline void cladefs_dax_thaw(struct cladefs_dax *dax) {}
static inline void cladefs_dax_destroy(struct cladefs_dax *dax) {}
static inline void cladefs_dax_show_stats(struct cladefs_dax *dax, struct seq_file *m) {}
static inline void cladefs_dax_inode_init(struct cladefs_dax_inode *di) {}
static inline bool cladefs_dax_set_inode(struct inode *inode, bool created)
{
	return false;
}
static inline void cladefs_dax_evict(struct inode *inode) {}
static inline void cladefs_dax_written(struct inode *inode) {}
static inline int cladefs_dax_write_back(struct inode *inode)
{
	return 0;
}
#endif

#endif
