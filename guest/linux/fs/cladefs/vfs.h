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
#include <linux/jump_label.h>
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
	/* cladefs: readahead's reads, off their readers (cladefs_vfs_readahead); a follower has none. */
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
	 * read-only ones too (cladefs_vfs_settle_frozen).
	 */
	unsigned int volumes;
	unsigned int volumes_frozen;
	/*
	 * cladefs: whether the filesystem is frozen, which takes all its volumes frozen: nothing is
	 * open on the device, so no volume mounts until it thaws, with the first to thaw. A thaw that
	 * failed leaves it frozen, as the VFS leaves the superblock, for the next to go on from where
	 * it stopped (cladefs_thaw, TASK-200).
	 */
	bool frozen;
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
	/*
	 * cladefs: a follower's (cladefs_follows, backlog doc-8): applies its writer's commits, as its
	 * device tells of them, off the interrupt (cladefs_follow).
	 */
	bool follows;
	struct work_struct follow;
	/*
	 * cladefs: the commits a follower's state is as of, as its readers may find it: every batch up
	 * to `follow_settled`'s commit settled, none past `follow_applied`'s applied
	 * (CLADEFS_IOC_FOLLOW_COMMITS). Written by its batches alone.
	 */
	u64 follow_settled;
	u64 follow_applied;
	/*
	 * cladefs: a follower's fill barrier (TASK-42.10, doc-8): what readers hold shared as they
	 * read its trees and fill the kernel's caches from them (cladefs_vfs_fill_begin), and a batch
	 * exclusively, from before it drops what the kernel keeps of what it changes until it has
	 * applied the change and set what the kernel keeps anew: no reader finds, or caches, part of
	 * a batch, nor a cache stale after one it found. Taken after an inode's i_rwsem, which the
	 * VFS holds as it calls in (a directory's, to look up or read it) and a read(2) takes, and
	 * mmap_lock, which a fault holds, and before an inode's invalidate_lock; a batch takes no
	 * i_rwsem. How long batches waited for it, and how many did (CLADEFS_IOC_FOLLOW_STATS).
	 */
	struct rw_semaphore fill_barrier;
	u64 follow_batches;
	u64 fill_barrier_wait_ns;
	u64 fill_barrier_wait_max_ns;
};

/*
 * cladefs: CLADEFS_IOC_FOLLOW_COMMITS, on a follower's directory: the commits its state is as of. A
 * read between two calls was of a state as of a commit from the first's `settled` to the second's
 * `applied` (the followers' history checker, TASK-42.7).
 */
struct cladefs_follow_commits {
	__u64 settled;
	__u64 applied;
};
#define CLADEFS_IOC_FOLLOW_COMMITS _IOR('x', 0x42, struct cladefs_follow_commits)

/*
 * cladefs: CLADEFS_IOC_FOLLOW_STATS, on a follower's directory: its batches, and how long they waited
 * for its fill barrier, all told and at most, in nanoseconds.
 */
struct cladefs_follow_stats {
	__u64 batches;
	__u64 barrier_wait_ns;
	__u64 barrier_wait_max_ns;
};
#define CLADEFS_IOC_FOLLOW_STATS _IOR('x', 0x43, struct cladefs_follow_stats)

/*
 * cladefs: CLADEFS_IOC_CACHED_OBJECTS, on any directory: how many records Fxfs caches, which the
 * superblock's shrinker frees under memory pressure (cladefs_cached_objects).
 */
#define CLADEFS_IOC_CACHED_OBJECTS _IOR('x', 0x44, __u64)

/*
 * cladefs: what publishes a shared volume, and makes one over it (backlog doc-10, Publishing). All
 * take CAP_SYS_ADMIN, on a directory, and none a follower (-EROFS).
 *
 * CLADEFS_IOC_FLUSH, on the volume's: puts all it holds in one layer, with its writers stopped by
 * FIFREEZE on it, to stay so. -EBUSY with the filesystem frozen, as with no other volume mounted.
 *
 * CLADEFS_IOC_MANIFEST, on the volume's, with the filesystem frozen (-EINVAL otherwise): writes what
 * describes it to `data`, which has room for `size` bytes (-ERANGE otherwise), and sets `size`.
 * CLADEFS_MANIFEST_MAX bytes are always enough. -EBUSY if the journal holds anything of the volume:
 * thaw, flush and freeze again. Its layers stay as a fork of the store takes them before the thaw.
 *
 * CLADEFS_IOC_IMPORT, on any of the filesystem's: makes the volume `name`, of `name_size` bytes, over
 * the `manifest_size` bytes at `manifest`. -EBUSY with the filesystem frozen.
 */
struct cladefs_manifest {
	__u64 data;
	__u64 size;
};
struct cladefs_import {
	__u64 name;
	__u64 name_size;
	__u64 manifest;
	__u64 manifest_size;
};
/* crates/fxfs's MAX_MANIFEST_SIZE. */
#define CLADEFS_MANIFEST_MAX 65536
#define CLADEFS_IOC_FLUSH _IO('x', 0x45)
#define CLADEFS_IOC_MANIFEST _IOWR('x', 0x46, struct cladefs_manifest)
#define CLADEFS_IOC_IMPORT _IOW('x', 0x47, struct cladefs_import)

static inline struct cladefs_vfs_info *cladefs_vfs_info(struct super_block *sb)
{
	return sb->s_fs_info;
}

static inline struct cladefs *cladefs_vfs_fs(struct super_block *sb)
{
	return cladefs_vfs_info(sb)->fs;
}

/*
 * cladefs: whether any filesystem follows its writer: a static key, so that a writer's mounts, with
 * no follower mounted, take no cell for the follower's paths.
 */
DECLARE_STATIC_KEY_FALSE(cladefs_vfs_followers);

/*
 * cladefs: the followers' torture test's knobs (TASK-42.7, doc-8 testing 6), module parameters
 * (cladefs.NAME=VALUE on the kernel's command line): delays, in microseconds, that widen the windows
 * the protocol closes; and mutations, each switching off the rule that closes one, which the test
 * must catch: a follower's batches dropping and settling nothing; its readers' fill barrier not
 * taken; its read(2)s' copies faulting, holding the barrier. All 0 but in the test.
 * follow_fail_batch has its batches fail once applied, as a read of the store would (TASK-79.23).
 */
extern unsigned int cladefs_follow_apply_delay_us;
extern unsigned int cladefs_follow_iomap_delay_us;
extern bool cladefs_follow_skip_drop;
extern bool cladefs_follow_skip_barrier;
extern bool cladefs_follow_read_faults;
extern bool cladefs_follow_fail_batch;

/* cladefs: whether `sb`'s filesystem follows its writer. */
static inline bool cladefs_vfs_follows(struct super_block *sb)
{
	return static_branch_unlikely(&cladefs_vfs_followers) && cladefs_vfs_info(sb)->follows;
}

/*
 * cladefs: a reader of a follower's `sb` holds its fill barrier shared from before it reads the
 * trees until what it read is in the kernel's caches, or copied out (vfs.h's cladefs_vfs_info); on a
 * writer, nothing.
 */
static inline void cladefs_vfs_fill_begin(struct super_block *sb)
{
	if (cladefs_vfs_follows(sb) && !cladefs_follow_skip_barrier)
		down_read(&cladefs_vfs_info(sb)->fill_barrier);
}

/* As cladefs_vfs_fill_begin, but only if no batch holds the barrier, or waits for it. */
static inline bool cladefs_vfs_fill_try(struct super_block *sb)
{
	return !cladefs_vfs_follows(sb) || cladefs_follow_skip_barrier ||
	       down_read_trylock(&cladefs_vfs_info(sb)->fill_barrier);
}

static inline void cladefs_vfs_fill_end(struct super_block *sb)
{
	if (cladefs_vfs_follows(sb) && !cladefs_follow_skip_barrier)
		up_read(&cladefs_vfs_info(sb)->fill_barrier);
}

/*
 * cladefs: a follower's read(2) of `iocb`'s file into `to` by `read`, a read_iter, its fill barrier
 * held a chunk at a time (super.c). A copy to the reader's memory that would fault, which may be a
 * mapping of a follower's file whose fault takes the barrier, stops short instead; the barrier let
 * go, the reader's pages are faulted in, and the read goes on.
 */
ssize_t cladefs_vfs_follower_read(struct kiocb *iocb, struct iov_iter *to,
			       ssize_t (*read)(struct kiocb *iocb, struct iov_iter *to));

/*
 * cladefs: a regular file's open and release, which hold it on a follower (cladefs_hold), for DAX or
 * not (super.c).
 */
int cladefs_vfs_file_open(struct inode *inode, struct file *file);
int cladefs_vfs_file_release(struct inode *inode, struct file *file);

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
	/*
	 * cladefs: a follower's directory's generation: how many batches changed its entries, which
	 * its dentries were looked up at (d_time) and revalidate against.
	 */
	atomic64_t gen;
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
 * cladefs_dax_quiesce stops what uses it, and cladefs_dax_destroy frees it.
 */
struct cladefs_dax_volume *cladefs_dax_add_volume(struct cladefs_dax *dax, struct cladefs *fs);
/*
 * Lets go of what `dax`'s window keeps of `volume`, its inodes evicted, its data synced, and frees
 * it: the window goes on for the other volumes.
 */
void cladefs_dax_remove_volume(struct cladefs_dax *dax, struct cladefs_dax_volume *volume);
/* Writes back every DAX file's dirty pages, before a sync: 0, or the first error. */
int cladefs_dax_settle(struct cladefs_dax *dax);
/*
 * cladefs: stops the budget's worker (dax.c), and waits for it, before a volume unmounts;
 * cladefs_dax_start starts it again, unless the filesystem is frozen.
 */
void cladefs_dax_stop(struct cladefs_dax *dax);
void cladefs_dax_start(struct cladefs_dax *dax);
/*
 * cladefs: the filesystem is frozen, its commits uploaded, nothing dirty: every private page is
 * rebound to its extent's, for a snapshot to take the window (doc-13). cladefs_dax_thaw ends it.
 */
void cladefs_dax_frozen(struct cladefs_dax *dax);
/*
 * Stops `dax`'s background work, once nothing writes, before the filesystem unmounts, or freezes;
 * cladefs_dax_thaw starts it again.
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
/*
 * cladefs: a follower's writer changed file `ino`'s data from byte `start` to `end`, inclusive,
 * which the window's ranges of its chunks there may map as it was: drops them, to be mapped again
 * as needed. Through `inode`, the file's, if one is read in, whose invalidate_lock the caller
 * holds for writing: it breaks the inode's mappings of them first.
 */
int cladefs_dax_changed(struct cladefs_dax *dax, struct cladefs_dax_volume *volume, struct inode *inode,
		     u64 ino, loff_t start, loff_t end);
/*
 * cladefs: a follower's DAX file, shrunk by its writer to `size`, its new size set: the guest's
 * mappings of its pages past it go, as a truncate's do. With its inode's invalidate_lock held for
 * writing.
 */
int cladefs_dax_shrunk(struct inode *inode, loff_t size);
#else
static inline int cladefs_dax_changed(struct cladefs_dax *dax, struct cladefs_dax_volume *volume,
				   struct inode *inode, u64 ino, loff_t start, loff_t end)
{
	return 0;
}
static inline int cladefs_dax_shrunk(struct inode *inode, loff_t size)
{
	return 0;
}
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
