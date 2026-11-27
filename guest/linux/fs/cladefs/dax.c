// SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause
// Copyright 2026 The cladefs Authors.
/*
 * Files' data mapped from the extent-store device's DAX window (backlog doc-2), as virtio-fs's DAX
 * maps a host's files (fs/fuse/dax.c), instead of the page cache: reads, writes and mappings of
 * files reach the extents the host shares, through the window.
 *
 * The window is split into ranges, each mapping a chunk of CLADEFS_DAX_SIZE bytes of one file. A
 * range is as large as its chunk of the file, in pages: a small file's takes what it needs of the
 * window, rounded up to a power of two, so that a file growing a page at a time maps its data again
 * only as often as it doubles.
 *
 * The window copies on write (backlog doc-13, TASK-164). It maps sealed extents, and zeros for
 * holes and pages past a file's end, read-only, nothing open: every page of a range has a window
 * page. A store to a page (a fault's, a write(2)'s copy) makes it private on the host, the VM's
 * own, holding what it held, at the same address. A page's state is dax.c's alone, its range's,
 * which only pages.c's transitions change (TASK-160.4): the iomap that dirties a page has the host
 * make it private first (PRIVATE), so that a page is dirty only if private, whenever its store
 * lands, and joins its file's dirty set, with room reserved in Fxfs for its writeback, counted
 * against the device's private_max (cladefs_dax_dirty). Writeback (fsync, sync, writepages, the
 * budget) swaps the set out, the writeback set, write-protects what faults made writable by the
 * page tables, and has Fxfs write the pages' bytes from the window to new extents, allocated then,
 * committing after them, as the page cache's writeback does (cladefs_dax_write_back). Once the barrier
 * after it has sealed those extents, a private page in neither set may be rebound to its
 * extent's page, the same bytes at the same address (cladefs_dax_rebind), under its file's semaphore
 * held for writing, which sees no store to it under way. An UNMAP, or a truncate's MAP of zeros,
 * ends what it covers (cladefs_dax_dropped). The kernel's DAX entries' dirty tags and locks decide
 * none of it: they are cleaned for the kernel's sake alone (cladefs_dax_clean_entries). Nothing moves,
 * so nothing seals but what Fxfs commits; files created in a mount are page-cache inodes until read
 * in again (cladefs_dax_set_inode).
 *
 * A chunk whose data is one piece of a sealed extent of at most a chunk's size, as small files'
 * are, sharing extents (the allocator's staging extents), is a slice of that extent's region
 * instead: the window's mapping of the whole extent, from its start, read-only, which all the files
 * it holds share. So a file read takes no request of its own, once its extent is mapped. A region
 * goes with its last slice. A write to a slice's chunk drops the slice (-EXDEV), and the chunk is
 * mapped again as a range of its own: a store to the region's page would change it for every file
 * sharing it.
 *
 * Ranges belong to a file (struct cladefs_dax_file, by inode number), not to an inode: they outlive
 * the inodes read in for the file, so that a file read again maps from the window what it mapped
 * before, with no request, as pmem stays mapped across the guest's dropped caches. A file's ranges
 * go as its data does (unlinked, truncated); others are reclaimed, least recently used, when the
 * window runs out.
 *
 * Locks are taken in this order: an inode's lock (read and write); an inode's invalidate_lock
 * (faults, truncation and reclaim); its file's writeback lock (`wb_lock`); then its file's DAX
 * semaphore. Another inode's are only tried, when reclaiming its ranges.
 *
 * cladefs: a file's writeback lock and semaphore are held only in a NOFS allocation scope, taken
 * and let go of through vfs.h's cladefs_nofs_* alone (scripts/cocci/nofs-locks.cocci), which check
 * that it is one. Reclaim's inode shrinker evicts inodes no one holds, and an eviction writes its
 * file back, taking both (cladefs_dax_evict): an allocation under them that could reclaim from the
 * filesystem, in dax.c, in Rust or in the device's driver, could wait on them itself, as the
 * budget's rebind's did in the driver's sg_alloc_table, holding a file whose inode it doesn't hold
 * (TASK-559). A function that takes and lets go of them enters the scope at its top
 * (cladefs_nofs_scope); one that returns holding them leaves it to its caller
 * (cladefs_dax_map_missing; cladefs_dax_resize_begin, whose caller is data.c's resize); and the
 * iomaps' callers enter it around them, a write's iomap holding the semaphore from its begin to its
 * end (cladefs_dax_iomap_rw).
 */

#include <linux/dax.h>
#include <linux/completion.h>
#include <linux/delay.h>
#include <linux/genalloc.h>
#include <linux/hash.h>
#include <linux/hashtable.h>
#include <linux/iomap.h>
#include <linux/log2.h>
#include <linux/mm.h>
#include <linux/pagemap.h>
#include <linux/pagewalk.h>
#include <linux/rmap.h>
#include <linux/slab.h>
#include <linux/sort.h>
#include <linux/uio.h>
#include <linux/vmalloc.h>
#include <linux/virtio_extent.h>
#include <linux/wait.h>
#include <linux/workqueue.h>
#include <linux/writeback.h>

#include "cladefs_trace.h"
#include "dax_internal.h"

/*
 * cladefs: the most pages one write's iomap makes private: an eighth of the device's private_max, if
 * less (cladefs_dax_iomap_write).
 */
#define CLADEFS_DAX_PRIVATE_PAGES 64UL
/*
 * cladefs: the pages a write(2) following on from its file's last makes private ahead of it, at
 * first and at most (cladefs_dax_write_follows, TASK-164.10).
 */
#define CLADEFS_DAX_AHEAD_FIRST 16U
#define CLADEFS_DAX_AHEAD_MOST 64U
/* cladefs: how far back in the last write a write may begin and follow on from it: a log's block. */
#define CLADEFS_DAX_FOLLOWS (16UL << 10)

/*
 * The window's space is handed out from a pool, page by page, at this offset: the pool takes no
 * address 0.
 */
#define CLADEFS_DAX_POOL_BASE (1UL << 48)

/* cladefs: how many waits of 10 ms with nothing let go of a range waits for room for (30 s). */
#define CLADEFS_DAX_NO_ROOM_WAITS 3000
/*
 * cladefs: how many times a write or a fault has a slice dropped, its chunk mapped again as a range
 * of its own, before giving up, as a reader may make it a slice again each time; and how many
 * reads of a range copy nothing, memory there, before one gives up.
 */
#define CLADEFS_DAX_MAX_DROPS 256

static const struct iomap_ops cladefs_dax_iomap_ops;

/*
 * cladefs: the `count` maps `maps`, in as few requests as the device takes: or, with
 * cladefs.dax_fail_map_many, the first alone, then a failure, as a device's MAP_MANY may.
 */
static int cladefs_dax_device_map_many(struct cladefs_dax *dax, const struct cladefs_device_map *maps,
				    unsigned int count)
{
	int err;

	if (count > 1 && cladefs_dax_should_fail_map_many()) {
		err = cladefs_device_map_many(dax->dev, maps, 1);
		return err ?: -EIO;
	}
	return cladefs_device_map_many(dax->dev, maps, count);
}

static const struct address_space_operations cladefs_dax_aops;
static const struct file_operations cladefs_dax_file_operations;
static const struct cladefs_data_ops cladefs_dax_data_ops;
static int cladefs_dax_break_layouts(struct inode *inode, loff_t start);
static int cladefs_dax_zero_tail(struct inode *inode, loff_t old, loff_t size);
static void cladefs_dax_put_region(struct cladefs_dax *dax, struct cladefs_dax_region *region);

static struct cladefs_dax *cladefs_dax_of(struct inode *inode)
{
	return cladefs_vfs_info(inode->i_sb)->dax;
}

static struct cladefs_dax_volume *cladefs_dax_volume_of(struct inode *inode)
{
	return cladefs_vfs_info(inode->i_sb)->dax_volume;
}

/* The file `inode` is read in for, whose ranges it uses. */
static struct cladefs_dax_file *cladefs_dax_file(struct inode *inode)
{
	return cladefs_vfs_inode(inode)->dax.file;
}

/*
 * Room for the pieces of a chunk's data, at most one per page, from Rust. Under the file's
 * semaphore, in the NOFS scope it is held in (cladefs_nofs_held), as the extents' lookups are.
 */
static struct cladefs_dax_extent *cladefs_dax_alloc_extents(struct cladefs_dax *dax, size_t capacity)
{
	struct cladefs_dax_extent *extents;

	/* DAX blocks are pages: at most one returned extent per covered logical page. */
	extents = kvmalloc_array(capacity, sizeof(*extents), GFP_KERNEL);
	if (extents)
		atomic64_add(capacity * sizeof(*extents), &dax->extent_alloc_bytes);
	return extents;
}

/*
 * cladefs: whether `range` holds pages an UNMAP, or a MAP over them, would lose: those of its file's
 * dirty set, which no extent holds yet. Under its file's semaphore.
 */
static bool cladefs_dax_range_dirty(const struct cladefs_dax_range *range)
{
	lockdep_assert_held(&range->file->sem);
	return cladefs_dax_pages_any_dirty(range->state);
}

/* cladefs: counts `private` more pages private, and `dirty` more dirty, of `file`'s, or fewer. */
static void cladefs_dax_count(struct cladefs_dax *dax, struct cladefs_dax_file *file, long private,
			   long dirty)
{
	if (private) {
		atomic_long_add(private, &dax->private);
		atomic_long_add(private, &file->private_pages);
	}
	if (dirty) {
		atomic_long_add(dirty, &dax->dirty);
		atomic_long_add(dirty, &file->dirty_pages);
	}
	/* Room among the private pages, for what the budget holds up. */
	if (private < 0)
		wake_up_all(&dax->budget_wait);
}

/*
 * cladefs: has the budget's worker run a round (cladefs_dax_work), which writes back and rebinds as the
 * private pages near private_max: and write back whatever is dirty, if `clean`, as a window whose
 * ranges are all dirty has nothing to reclaim. Not while it stops. It only queues the round.
 *
 * The round is queued under the lock cladefs_dax_stop says that the worker stops under, read
 * again there: a kick that read it before a stop, and queued the round after the stop's
 * cancel_work_sync, had the worker run on a filesystem unmounting or frozen (board 10373). A
 * pause point between the two reads (CLADEFS_DAX_STEP_KICK), where a kicker beside a stop may be
 * held up.
 */
void cladefs_dax_kick(struct cladefs_dax *dax, bool clean)
{
	if (clean)
		atomic_set(&dax->clean, 1);
	if (READ_ONCE(dax->stops))
		return;
	cladefs_dax_pause_file(CLADEFS_DAX_STEP_KICK, 0);
	mutex_lock(&dax->kick_lock);
	if (!dax->stops)
		queue_work(dax->wq, &dax->work);
	mutex_unlock(&dax->kick_lock);
}

/*
 * cladefs: counts what a transition of pages whose data goes took (doc-13): an UNMAP, or a
 * truncate's MAP of zeros, ended `took`'s private pages of `range`: their dirty pages' room
 * released, dropped unwritten, as a truncate's, or an unlinked file's are; and, once the host has
 * freed them (`freed`), their private pages no longer counted. Not before the request ends: the
 * budget let another's store have the room, which the host, holding the pages still, refused past
 * private_max (guest/sim, seed 90). Nor if it failed, the host holding them for good: counted
 * still, lost, as the window's pages an UNMAP failed for are (cladefs_dax_give_unmapped). With its
 * file's semaphore held for writing.
 */
static void cladefs_dax_dropped(struct cladefs_dax *dax, struct cladefs_dax_range *range,
			     struct cladefs_dax_change took, bool freed)
{
	struct cladefs_dax_file *file = range->file;
	struct cladefs_dax_volume *volume;

	lockdep_assert_held_write(&range->file->sem);
	if (took.dirty && !cladefs_dax_range_dirty(range))
		xa_clear_mark(&file->ranges, range->chunk, XA_MARK_0);
	cladefs_dax_count(dax, file, freed ? took.private : 0, took.dirty);
	if (!freed)
		atomic_long_add(took.private, &file->private_pages);
	/* A volume going (cladefs_dax_remove_volume) was synced: nothing of it is dirty. */
	volume = READ_ONCE(file->volume);
	if (took.dirty && !WARN_ON_ONCE(!volume))
		cladefs_release_dirty(volume->fs, file->ino, (u64)-took.dirty << PAGE_SHIFT);
}

/*
 * cladefs: the transition `map`, of `range`'s pages, made, once it mapped them: a range's own map,
 * of pages it maps nothing at (cladefs_dax_pages_fill); or a rebind's (`rebind`), of private, clean
 * pages, written back and sealed, private no more, the host having freed them for the extent's
 * page, or zeros, holding the same bytes (cladefs_dax_pages_rebind), counted: how many it rebound.
 * With the file's semaphore held for writing.
 */
static long cladefs_dax_mapped(struct cladefs_dax *dax, struct cladefs_dax_range *range,
			    const struct cladefs_device_map *map, bool rebind)
{
	unsigned long page = (map->window_offset - range->window_offset) >> PAGE_SHIFT;
	unsigned long n = map->len >> PAGE_SHIFT;
	long rebound;

	lockdep_assert_held_write(&range->file->sem);
	cladefs_dax_trace_map(dax, range, map);
	if (!rebind) {
		cladefs_dax_pages_fill(range->state, page, n, map->zeros);
		return 0;
	}
	rebound = cladefs_dax_pages_rebind(range->state, page, n, map->zeros);
	if (rebound)
		cladefs_dax_count(dax, range->file, -rebound, 0);
	return rebound;
}

/*
 * cladefs: maps the `count` maps `maps`, of `range`'s pages, in as few requests as the device takes,
 * or one by one if that fails, to say which: a sealed extent the device doesn't hold (-ENOENT),
 * never written, as zeros, a hole, as Fxfs reads it. An
 * extent open still (-EPERM: the window maps none), Fxfs having taken it for sealed, as one minted
 * before a seal and written after it is (storage_device's OpenExtents), fails them, unless a
 * `rebind`'s, when it is left as it is, its pages private. Has the range say what it mapped, and a
 * rebind's pages private no more (cladefs_dax_mapped): how many that rebound, or the first error. A
 * range's own maps are of pages it maps nothing at, none private. With its file's semaphore held
 * for writing. cladefs: with cladefs.dax_fail_maps, a map of a range's pages (not a rebind's) of more
 * than one piece maps the first, then fails, as a device's MAP of the second may; with
 * cladefs.dax_fail_fallbacks, the MAPs one by one after a failed MAP_MANY fail past the first
 * (TASK-164.2).
 */
static long cladefs_dax_map_all(struct cladefs_dax *dax, struct cladefs_dax_range *range,
			     struct cladefs_device_map *maps, size_t count, bool rebind)
{
	long rebound = 0;
	size_t i;
	int err;

	lockdep_assert_held_write(&range->file->sem);
	if (!count)
		return 0;
	if (rebind && cladefs_dax_should_fail_rebind_open()) {
		atomic64_inc(&dax->open_maps);
		return 0;
	}
	if (count > 1 && !rebind && cladefs_dax_should_fail_maps()) {
		rebound = cladefs_dax_map_all(dax, range, maps, 1, rebind);
		range->unsure = true;
		return rebound < 0 ? rebound : -EIO;
	}
	if (trace_cladefs_dax_map_enabled()) {
		u64 len = 0;

		for (i = 0; i < count; i++)
			len += maps[i].len;
		trace_cladefs_dax_map(rebind ? CLADEFS_TRACE_MAP_REBIND : CLADEFS_TRACE_MAP_RANGE,
				   range->file->ino, maps[0].window_offset, len, count);
	}
	err = cladefs_dax_device_map_many(dax, maps, count);
	atomic64_add(count, &dax->maps[CLADEFS_DAX_RANGE]);
	for (i = 0; i < count; i++) {
		struct cladefs_device_map *map = &maps[i];

		if (err) {
			/* If they failed, mapped again one by one, to say which, and why. */
			int one = i && cladefs_dax_should_fail_fallbacks() ? -EIO :
				  map->zeros ?
				  cladefs_device_map_zeros(dax->dev, map->window_offset, map->len) :
				  cladefs_device_map(dax->dev, map->cell, map->sequence, map->offset,
						  map->len, map->window_offset, map->prefault);

			if (one == -ENOENT && !map->zeros) {
				map->zeros = true;
				one = cladefs_device_map_zeros(dax->dev, map->window_offset, map->len);
			}
			if (one == -EPERM)
				atomic64_inc(&dax->open_maps);
			if (one == -EPERM && rebind)
				continue;
			if (one) {
				pr_warn("cladefs: mapping %llu bytes at %llu of %s at %llu of the DAX window failed: %d\n",
					map->len, map->offset, map->zeros ? "zeros" : "an extent",
					map->window_offset, one);
				/* What the MAP_MANY mapped of those after it, it doesn't say. */
				range->unsure = true;
				return one;
			}
		}
		atomic64_add(map->len, &dax->map_bytes[CLADEFS_DAX_RANGE]);
		rebound += cladefs_dax_mapped(dax, range, map, rebind);
	}
	return rebound;
}

/* cladefs: whether any of the pieces `extents` is of an extent not known to be sealed. */
static bool cladefs_dax_any_unsealed(const struct cladefs_dax_extent *extents, ssize_t count)
{
	ssize_t i;

	for (i = 0; i < count; i++) {
		if (extents[i].access != CLADEFS_DAX_SEALED)
			return true;
	}
	return false;
}

/*
 * cladefs: the pieces of file `ino`'s `len` bytes at `offset`, as cladefs_dax_extents, once no seal was
 * in progress as Fxfs looked: one says sealed from its beginning of what it has yet to seal on the
 * device (OpenExtents::seal, as cladefs-linux's Seal::begin_with has it), which the host refuses to
 * map (-EPERM). Looked up again after it, as long as seals keep coming: mapping them, a range's
 * map sealed and tried again, and a rebind left them private, in the cow_ scenarios' schedules a
 * mutant moved (cow_map_during_seal). A slice's region, likewise (cladefs_dax_new_slice).
 */
static ssize_t cladefs_dax_sealed_extents(struct cladefs_dax *dax, struct cladefs *fs, u64 ino,
				       loff_t offset, u64 len, struct cladefs_dax_extent *found,
				       size_t capacity)
{
	ssize_t count;
	u64 seals;

	for (;;) {
		seals = cladefs_device_seals(dax->dev);
		count = cladefs_dax_extents(fs, ino, offset, len, found, capacity);
		if (count < 0 || (!(seals & 1) && cladefs_device_seals(dax->dev) == seals))
			return count;
		cladefs_device_wait_unsealed(dax->dev);
	}
}

/*
 * cladefs: the maps of `range`'s pages `first` to `last`, exclusive, in `maps`, from the pieces
 * `found` of the file's bytes there, `count` of them, sorted, which may reach past them: each
 * sealed piece's pages from its extent, zeros where the file has none, a hole or past its end.
 * Prefaults (MAP_PREFAULT) if `prefault`. As
 * many as 2 * (`last` - `first`) + 1 at most: how many.
 */
static size_t cladefs_dax_maps_of(struct cladefs_dax *dax, struct cladefs_dax_range *range,
			       const struct cladefs_dax_extent *found, ssize_t count,
			       unsigned long first, unsigned long last, bool prefault,
			       struct cladefs_device_map *maps)
{
	loff_t start = cladefs_dax_chunk_start(range);
	unsigned long page = first;
	size_t n = 0;
	ssize_t i;

	for (i = 0; i <= count; i++) {
		const struct cladefs_dax_extent *e = i < count ? &found[i] : NULL;
		unsigned long from = last, to;
		u64 at;

		if (e)
			from = clamp_t(unsigned long, (e->offset - start) >> PAGE_SHIFT, page, last);
		/* The hole before it, or past the last: zeros. */
		if (from > page)
			maps[n++] = (struct cladefs_device_map) {
				.window_offset = cladefs_dax_page_offset(range, page),
				.len = (u64)(from - page) << PAGE_SHIFT,
				.zeros = true,
			};
		if (!e || from >= last)
			break;
		to = clamp_t(unsigned long, (e->offset + e->len - start) >> PAGE_SHIFT, from, last);
		at = start + ((u64)from << PAGE_SHIFT);
		if (to > from && e->access == CLADEFS_DAX_SEALED)
			maps[n++] = (struct cladefs_device_map) {
				.cell = e->cell, .sequence = e->sequence,
				.offset = e->extent_offset + (at - e->offset),
				.len = (u64)(to - from) << PAGE_SHIFT,
				.window_offset = cladefs_dax_page_offset(range, from),
				.prefault = prefault,
			};
		page = max(page, to);
	}
	return n;
}

/*
 * cladefs: maps `range`'s pages `first` to `last`, exclusive, read-only (doc-13): each from the
 * sealed extent holding the file's bytes there, or zeros where it has none, holes and pages past
 * its end, so that every page of a range has a window page, which the iomap that dirties it makes
 * private, and no write is new (IOMAP_F_NEW). Nothing maps an open extent: what is open is sealed first (cladefs_fsync), as a
 * page-cache inode's writes read in before their seal, or a growth's copy of the last block, may
 * leave; rebinding private pages, as their writeback left them, is cladefs_dax_rebind_range's, which
 * leaves those whose extent is open private. 0, or a -errno. With the file's semaphore held for
 * writing. A read's pages are prefaulted (MAP_PREFAULT, `prefault`), which the host installs and
 * maps for the guest at once; a write's are named alone, which the host installs as they are first
 * touched, a store's PRIVATE putting its private copy in place of nothing, where a page installed
 * first took a TLB flush to replace (doc-13, TASK-276.6).
 */
static long cladefs_dax_map_pages(struct cladefs_dax *dax, struct cladefs *fs, struct cladefs_dax_range *range,
			       unsigned long first, unsigned long last, bool prefault)
{
	loff_t start = cladefs_dax_chunk_start(range);
	size_t capacity = last - first, n;
	struct cladefs_dax_extent *found;
	struct cladefs_device_map *maps;
	unsigned int tries;
	ssize_t count;
	long ret = 0;

	lockdep_assert_held_write(&range->file->sem);
	/*
	 * cladefs: a slice's window pages belong to its shared region (cladefs_dax_region_map);
	 * mapping over them here would overwrite sibling slices (board 2508, TASK-556).
	 */
	if (WARN_ON_ONCE(range->region))
		return -EIO;
	if (!capacity)
		return 0;
	found = cladefs_dax_alloc_extents(dax, capacity);
	maps = kmalloc_array(2 * capacity + 1, sizeof(*maps), GFP_NOFS);
	if (!found || !maps) {
		ret = -ENOMEM;
		goto out;
	}
	for (tries = 0;; tries++) {
		count = cladefs_dax_sealed_extents(dax, fs, range->file->ino,
						start + ((loff_t)first << PAGE_SHIFT),
						(u64)capacity << PAGE_SHIFT, found, capacity);
		if (count < 0) {
			ret = count;
			goto out;
		}
		if (!cladefs_dax_any_unsealed(found, count)) {
			n = cladefs_dax_maps_of(dax, range, found, count, first, last, prefault, maps);
			ret = cladefs_dax_map_all(dax, range, maps, n, false);
			if (ret != -EPERM)
				break;
		}
		/*
		 * Each seal seals what was written before it began, which Fxfs committed before it
		 * reads the file's pieces: one is enough, unless writes go on meanwhile.
		 */
		if (tries == 3) {
			pr_warn("cladefs: ino %llu chunk %lu: extents open after %u seals\n",
				range->file->ino, range->chunk, tries);
			ret = -EIO;
			break;
		}
		atomic64_inc(&dax->seals_made);
		ret = cladefs_fsync(fs);
		if (ret)
			break;
	}
out:
	kfree(maps);
	kvfree(found);
	return ret;
}

/*
 * cladefs: rebinds `range`'s private, clean pages (cladefs_dax_rebind): every run of them in as few
 * requests as the device takes (MAP_MANY), from one look at the file's pieces over them all, where
 * each run was a look and a MAP of its own. A forked pgbench's writes scattered over its tables
 * left runs of a page or two: 30,085 MAPs a minute, 294 us each, nested, the file's semaphore held
 * for writing through all of a file's (TASK-164.10, GCE r649). A page whose extent is open yet
 * stays private (cladefs_dax_map_all), counted in `*open`. How many it rebound, or a -errno. With the
 * file's semaphore held for writing, and its writeback lock.
 */
static long cladefs_dax_rebind_range(struct cladefs_dax *dax, struct cladefs *fs,
				  struct cladefs_dax_range *range, unsigned long *open)
{
	loff_t start = cladefs_dax_chunk_start(range);
	unsigned long first, last = 0, page, end, rebindable = 0;
	struct cladefs_dax_extent *found;
	struct cladefs_device_map *maps;
	size_t capacity, runs = 0, n = 0;
	ssize_t count, i = 0;
	long ret;

	lockdep_assert_held_write(&range->file->sem);
	lockdep_assert_held(&range->file->wb_lock);
	first = cladefs_dax_pages_next_rebindable(range->state, 0, &end);
	for (page = first; page < range->pages;
	     page = cladefs_dax_pages_next_rebindable(range->state, end, &end)) {
		last = end;
		rebindable += end - page;
		runs++;
	}
	if (!runs)
		return 0;
	capacity = last - first;
	found = cladefs_dax_alloc_extents(dax, capacity);
	/* A run's maps are twice its pages and one at most. */
	maps = kmalloc_array(2 * capacity + runs, sizeof(*maps), GFP_NOFS);
	if (!found || !maps) {
		ret = -ENOMEM;
		goto out;
	}
	/* Before the MAP over them, holding what a rebind holds (pause points). */
	cladefs_dax_pause_file(CLADEFS_DAX_STEP_REBIND, range->file->ino);
	count = cladefs_dax_sealed_extents(dax, fs, range->file->ino,
					start + ((loff_t)first << PAGE_SHIFT),
					(u64)capacity << PAGE_SHIFT, found, capacity);
	if (count < 0) {
		ret = count;
		goto out;
	}
	for (page = cladefs_dax_pages_next_rebindable(range->state, first, &end); page < range->pages;
	     page = cladefs_dax_pages_next_rebindable(range->state, end, &end)) {
		/* The pieces before the run, done with: the runs come in order. */
		while (i < count &&
		       found[i].offset + found[i].len <= start + ((u64)page << PAGE_SHIFT))
			i++;
		n += cladefs_dax_maps_of(dax, range, found + i, count - i, page, end, false, maps + n);
	}
	ret = cladefs_dax_map_all(dax, range, maps, n, true);
	/* Every page clean and private under the semaphore is rebound, but those of open extents. */
	if (ret >= 0)
		*open += rebindable - ret;
out:
	kfree(maps);
	kvfree(found);
	return ret;
}

/* Wakes those waiting for a range to be freed, or for its last user to be done. */
static void cladefs_dax_released(struct cladefs_dax *dax)
{
	atomic_long_inc(&dax->released);
	/* Pairs the condition update with a waiter preparing to sleep before testing the queue. */
	if (wq_has_sleeper(&dax->wait))
		wake_up_all(&dax->wait);
}

/* Waits up to `ms` milliseconds for a range to be freed, or its last user done, since `seen`. */
static long cladefs_dax_wait_released(struct cladefs_dax *dax, long seen, unsigned int ms)
{
	return wait_event_killable_timeout(dax->wait, atomic_long_read(&dax->released) != seen,
					   msecs_to_jiffies(ms));
}

/*
 * `pages` pages of the window's free space: their offset in the window, or -ENOSPC if it has no
 * room. A whole chunk's range is at a chunk's alignment, if `aligned`.
 */
static long cladefs_dax_take_window(struct cladefs_dax *dax, unsigned long pages, bool aligned)
{
	size_t size = pages << PAGE_SHIFT;
	unsigned long addr;

	if (aligned && pages == CLADEFS_DAX_PAGES)
		addr = gen_pool_alloc_algo(dax->pool, size, gen_pool_first_fit_order_align, NULL);
	else
		addr = gen_pool_alloc(dax->pool, size);
	if (!addr)
		atomic_long_inc(&dax->window_full);
	return addr ? addr - CLADEFS_DAX_POOL_BASE : -ENOSPC;
}

/*
 * cladefs: `pages` pages of the window's space for regions: their offset in the window, or -EAGAIN
 * if it has no room now, as cladefs_dax_get_region says. Regions map extents of any length at any
 * offset: in the ranges' space, they left it free in pieces too small for a chunk's range, and
 * every range reclaimed others, and mapped its chunk again (TASK-91.10).
 */
static long cladefs_dax_take_region_window(struct cladefs_dax *dax, unsigned long pages)
{
	unsigned long addr = gen_pool_alloc(dax->region_pool, pages << PAGE_SHIFT);

	if (!addr)
		atomic_long_inc(&dax->regions_full);
	return addr ? addr - CLADEFS_DAX_POOL_BASE : -EAGAIN;
}

/* Frees `pages` pages of the window at `window_offset`, which nothing maps. */
static void cladefs_dax_give_window(struct cladefs_dax *dax, u64 window_offset, unsigned long pages)
{
	struct gen_pool *pool = window_offset < dax->regions_at ? dax->pool : dax->region_pool;

	if (pages)
		gen_pool_free(pool, window_offset + CLADEFS_DAX_POOL_BASE, pages << PAGE_SHIFT);
}

/*
 * cladefs: frees `pages` pages of the window at `window_offset`, which an UNMAP unmapped, unless it
 * failed (`err`): the host may map them still, which the range or region given them next would
 * read wherever it reads a page it hasn't mapped, as would a user's mapping the kernel didn't
 * replace. They are kept from use for good, lost to the window (TASK-44.22).
 */
static void cladefs_dax_give_unmapped(struct cladefs_dax *dax, u64 window_offset, unsigned long pages,
				   int err)
{
	if (!err) {
		cladefs_dax_give_window(dax, window_offset, pages);
		return;
	}
	pr_warn_ratelimited("cladefs: unmapping the DAX window at %llx failed: %d: its %lu pages kept from use\n",
			    window_offset, err, pages);
	atomic64_add((u64)pages << PAGE_SHIFT, &dax->lost_bytes);
}

/* Frees what `range` holds in memory, and it. */
static void cladefs_dax_free_range_memory(struct cladefs_dax_range *range)
{
	cladefs_dax_pages_free(range->state);
	kfree(range);
}

/*
 * A new range of `pages` pages at `window_offset`, unmapped and on no list: of its own, or a slice
 * of `region`, whose slice it takes.
 */
static struct cladefs_dax_range *cladefs_dax_new_range(u64 window_offset, unsigned long pages,
						 struct cladefs_dax_region *region)
{
	struct cladefs_dax_range *range;

	range = kzalloc(sizeof(*range), GFP_NOFS);
	if (!range)
		return ERR_PTR(-ENOMEM);
	range->state = cladefs_dax_pages_new(pages);
	if (!range->state) {
		cladefs_dax_free_range_memory(range);
		return ERR_PTR(-ENOMEM);
	}
	range->window_offset = window_offset;
	range->region = region;
	range->pages = pages;
	INIT_LIST_HEAD(&range->list);
	return range;
}

static u64 cladefs_dax_region_key(u64 cell, u64 sequence)
{
	return sequence ^ (cell * GOLDEN_RATIO_64);
}

/* The region mapping the extent {cell, sequence}, if there is one. With the regions lock held. */
static struct cladefs_dax_region *cladefs_dax_find_region(struct cladefs_dax *dax, u64 cell,
						    u64 sequence)
{
	struct cladefs_dax_region *region;

	lockdep_assert_held(&dax->regions_lock);

	hash_for_each_possible(dax->regions, region, node, cladefs_dax_region_key(cell, sequence)) {
		if (region->cell == cell && region->sequence == sequence)
			return region;
	}
	return NULL;
}

/*
 * Drops a reference to `region`: with its last, the region is unmapped, and its space free again.
 */
static void cladefs_dax_put_region(struct cladefs_dax *dax, struct cladefs_dax_region *region)
{
	int err = 0;

	spin_lock(&dax->regions_lock);
	if (--region->refs) {
		spin_unlock(&dax->regions_lock);
		return;
	}
	hash_del(&region->node);
	spin_unlock(&dax->regions_lock);
	if (region->mapped)
		err = cladefs_device_unmap(dax->dev, region->window_offset,
					(u64)region->mapped << PAGE_SHIFT);
	cladefs_dax_give_unmapped(dax, region->window_offset, region->pages, err);
	kfree(region);
	atomic_long_dec(&dax->region_count);
	cladefs_dax_released(dax);
}

static void cladefs_dax_free_range_rcu(struct rcu_head *rcu)
{
	cladefs_dax_free_range_memory(container_of(rcu, struct cladefs_dax_range, rcu));
}

/*
 * Frees `range`, which nothing maps, and is in no file's ranges: its space in the window is free
 * again, unless its UNMAP failed with `unmap_err` (cladefs_dax_give_unmapped), or its slice is its
 * no more.
 */
static void cladefs_dax_free_range(struct cladefs_dax *dax, struct cladefs_dax_range *range, int unmap_err)
{
	spin_lock(&dax->lock);
	list_del(&range->list);
	spin_unlock(&dax->lock);
	if (range->region)
		cladefs_dax_put_region(dax, range->region);
	else
		cladefs_dax_give_unmapped(dax, range->window_offset, range->pages, unmap_err);
	call_rcu(&range->rcu, cladefs_dax_free_range_rcu);
	cladefs_dax_released(dax);
}

/*
 * Unmaps `range` and frees it, with its file's DAX semaphore held for writing, once nothing maps
 * its pages: its file's inode's mappings of them are gone (cladefs_dax_release), if it has one. A
 * slice's pages are its region's, which go with its last slice. cladefs: and no iomap or writeback
 * holds it (`users`), which reads the window at its place, given to another range next: WARNs if
 * one does, whichever way frees it (doc-13, GCE r544).
 */
static void cladefs_dax_unmap_range(struct cladefs_dax *dax, struct cladefs_dax_range *range)
{
	bool mapped = !range->region &&
		      (range->unsure || cladefs_dax_pages_any_mapped(range->state, 0, range->pages));
	int err = 0;

	lockdep_assert_held_write(&range->file->sem);
	WARN(atomic_read(&range->users),
	     "cladefs: ino %llu chunk %lu's range at %llx of the DAX window freed, %d using it\n",
	     range->file->ino, range->chunk, range->window_offset, atomic_read(&range->users));

	xa_erase(&range->file->ranges, range->chunk);
	if (mapped)
		err = cladefs_device_unmap(dax->dev, range->window_offset,
					(u64)range->pages << PAGE_SHIFT);
	/* cladefs: the UNMAP freed its private pages, unless it failed. */
	cladefs_dax_dropped(dax, range, cladefs_dax_pages_unmap(range->state, 0, range->pages), !err);
	cladefs_dax_free_range(dax, range, err);
}

/* Drops a reference to `file`, freeing it once it has none. */
static void cladefs_dax_put_file(struct cladefs_dax *dax, struct cladefs_dax_file *file)
{
	if (!refcount_dec_and_test(&file->refs))
		return;
	xa_destroy(&file->ranges);
	kfree(file);
}

/*
 * Forgets `file` if no inode is read in for it and it has no ranges, with the files lock held:
 * returns whether it did, when the caller drops the filesystem's reference to it.
 */
static bool cladefs_dax_forget_file(struct cladefs_dax *dax, struct cladefs_dax_file *file)
{
	lockdep_assert_held(&dax->files_lock);
	if (file->inode || !xa_empty(&file->ranges) || !file->volume ||
	    xa_load(&file->volume->files, file->ino) != file)
		return false;
	xa_erase(&file->volume->files, file->ino);
	return true;
}

/*
 * cladefs: cleans the kernel's dirty tags of `inode`'s DAX entries from `start` to `end`, for the
 * kernel's sake alone: a write fault tags its page's entry, and nothing of dax.c reads the tags,
 * its dirty sets saying what to write back, and its writeback write-protecting by the page tables
 * (cladefs_dax_write_back). But a mapping tagged dirty keeps its inode dirty
 * (__writeback_single_inode sets I_DIRTY_PAGES again), so the flusher's for ever and off the inode
 * LRU (writepages); and invalidate_inode_pages2_range refuses an entry tagged so
 * (cladefs_dax_release). An entry a fault holds, dax_writeback_one passes by, tagged still, which the
 * next writepages, or the range's release, with no fault under way, cleans. Its PTEs are
 * write-protected with it: a store after faults again, its page dirty already.
 */
static int cladefs_dax_clean_entries(struct inode *inode, loff_t start, loff_t end)
{
	struct writeback_control clean = {
		.sync_mode = WB_SYNC_ALL,
		.nr_to_write = LONG_MAX,
		.range_start = start,
		.range_end = end,
	};

	return dax_writeback_mapping_range(inode->i_mapping, cladefs_dax_of(inode)->dax_dev, &clean);
}

/*
 * Removes the inode's mappings of the chunk `range` maps, and the DAX entries for them, unless
 * a page is in use (pinned): with the inode's invalidate_lock held for writing. cladefs: and its
 * file's semaphore. Unless the range holds dirty pages, which its UNMAP would lose: their
 * writeback (cladefs_dax_write_back) takes the semaphore.
 */
static bool cladefs_dax_release(struct inode *inode, struct cladefs_dax_range *range)
{
	struct address_space *mapping = inode->i_mapping;
	loff_t start = cladefs_dax_chunk_start(range);
	loff_t end = start + ((loff_t)range->pages << PAGE_SHIFT) - 1;

	lockdep_assert_held(&range->file->sem);
	if (cladefs_dax_range_dirty(range))
		return false;
	if (dax_layout_busy_page_range(mapping, start, end))
		return false;
	/*
	 * Dirty entries go only once cleaned. cladefs: with no page dirty, an entry tagged dirty is the
	 * kernel's alone, cleaned here, which the invalidate_lock makes whole, no fault holding an
	 * entry: one a writeback left tagged, a fault holding it, failed the invalidation for ever,
	 * and a fault waiting for room reclaimed the range again and again (guest/sim, seed 3390 at
	 * 1,000 operations).
	 */
	if (cladefs_dax_clean_entries(inode, start, end))
		return false;
	return !invalidate_inode_pages2_range(mapping, start >> PAGE_SHIFT, end >> PAGE_SHIFT);
}

/*
 * Reclaims `file`'s range of chunk `chunk`, if no iomap uses it and its file's locks are free,
 * and returns whether it did. A file with no inode has nothing mapping its ranges but the window.
 */
static bool cladefs_dax_reclaim_range(struct cladefs_dax *dax, struct cladefs_dax_file *file,
				   pgoff_t chunk)
{
	struct cladefs_dax_range *range;
	struct inode *inode = NULL;
	bool reclaimed = false, forgotten = false;
	cladefs_nofs_scope();

	mutex_lock(&dax->files_lock);
	if (file->inode)
		inode = igrab(file->inode);
	mutex_unlock(&dax->files_lock);

	if (inode) {
		if (down_write_trylock(&inode->i_mapping->invalidate_lock)) {
			if (cladefs_nofs_down_write_trylock(&file->sem)) {
				range = xa_load(&file->ranges, chunk);
				if (range && !atomic_read(&range->users) &&
				    cladefs_dax_release(inode, range)) {
					cladefs_dax_unmap_range(dax, range);
					reclaimed = true;
				}
				cladefs_nofs_up_write(&file->sem);
			}
			filemap_invalidate_unlock(inode->i_mapping);
		}
		iput(inode);
		return reclaimed;
	}
	if (!cladefs_nofs_down_write_trylock(&file->sem))
		return false;
	mutex_lock(&dax->files_lock);
	/* An inode read in since may map the range: it is reclaimed with it next time. */
	if (!file->inode) {
		range = xa_load(&file->ranges, chunk);
		/* cladefs: an evicted inode's were written back: kept if not. */
		if (range && !atomic_read(&range->users) && !cladefs_dax_range_dirty(range)) {
			cladefs_dax_unmap_range(dax, range);
			reclaimed = true;
		}
		forgotten = cladefs_dax_forget_file(dax, file);
	}
	mutex_unlock(&dax->files_lock);
	cladefs_nofs_up_write(&file->sem);
	if (forgotten)
		cladefs_dax_put_file(dax, file);
	return reclaimed;
}

/* Reclaims the least recently used range no iomap uses, and returns whether it did. */
static bool cladefs_dax_reclaim(struct cladefs_dax *dax)
{
	struct cladefs_dax_file *file = NULL;
	struct cladefs_dax_range *range;
	pgoff_t chunk = 0;
	bool reclaimed;

	spin_lock(&dax->lock);
	list_for_each_entry(range, &dax->used, list) {
		if (atomic_read(&range->users))
			continue;
		/* Tried once: the next try starts with another. */
		list_move_tail(&range->list, &dax->used);
		file = range->file;
		/* Kept while it is tried: a range on the list keeps its file. */
		refcount_inc(&file->refs);
		chunk = range->chunk;
		break;
	}
	spin_unlock(&dax->lock);
	if (!file)
		return false;
	reclaimed = cladefs_dax_reclaim_range(dax, file, chunk);
	trace_cladefs_dax_reclaim(file->ino, chunk, reclaimed);
	cladefs_dax_put_file(dax, file);
	if (reclaimed)
		atomic_long_inc(&dax->reclaims);
	return reclaimed;
}

/*
 * `pages` pages of the window, as cladefs_dax_take_window, reclaimed if it has no room: their offset.
 * Waits for room, unless `nowait` asks, as a fault does, which can't: -EAGAIN then.
 */
static long cladefs_dax_alloc_window(struct cladefs_dax *dax, unsigned long pages, bool aligned,
				  bool nowait)
{
	long window_offset, seen, waited;
	unsigned int idle = 0;

	for (;;) {
		seen = atomic_long_read(&dax->released);
		window_offset = cladefs_dax_take_window(dax, pages, aligned);
		if (window_offset >= 0)
			return window_offset;
		if (cladefs_dax_reclaim(dax))
			continue;
		/*
		 * cladefs: nothing to reclaim, a chunk's range takes the room the window has off a
		 * chunk's alignment, mapped a page at a time (fs/dax.c falls back from a PMD where
		 * the window's page isn't 2 MiB-aligned): UNMAPs that failed keep a piece of an
		 * aligned slot from use for good, and every chunk's write failed ENOSPC once each
		 * slot had one, most of the window free (TASK-149.33).
		 */
		if (aligned && pages == CLADEFS_DAX_PAGES) {
			window_offset = cladefs_dax_take_window(dax, pages, false);
			if (window_offset >= 0) {
				atomic_long_inc(&dax->unaligned);
				return window_offset;
			}
		}
		/* cladefs: ranges holding dirty pages aren't reclaimed: written back, they are. */
		cladefs_dax_kick(dax, true);
		if (nowait)
			return -EAGAIN;
		/* Ranges in use, or whose inodes are locked, free up soon. */
		waited = cladefs_dax_wait_released(dax, seen, 10);
		if (waited < 0)
			return -EINTR;
		idle = waited ? 0 : idle + 1;
		/* cladefs: nothing let go of for so long isn't going to: space lost, or all pinned. */
		if (idle > CLADEFS_DAX_NO_ROOM_WAITS) {
			pr_warn_ratelimited("cladefs: no room in the window for %lu pages\n", pages);
			return -ENOSPC;
		}
	}
}

/* A new range of its own of `pages` pages, as cladefs_dax_alloc_window finds room for it. */
static struct cladefs_dax_range *cladefs_dax_alloc_range(struct cladefs_dax *dax, unsigned long pages,
						   bool fault)
{
	long window_offset = cladefs_dax_alloc_window(dax, pages, true, fault);
	struct cladefs_dax_range *range;

	if (window_offset < 0)
		return ERR_PTR(window_offset);
	range = cladefs_dax_new_range(window_offset, pages, NULL);
	if (IS_ERR(range))
		cladefs_dax_give_window(dax, window_offset, pages);
	return range;
}

/*
 * The bytes the region of extent `e` spans: the extent's, or a chunk's if Fxfs doesn't count it, as
 * a volume over a base counts none of the base's, whose filesystem's allocator does: as far as a
 * slice's extent may reach, a small file's staging extent's (cladefs_dax_new_slice). The device maps
 * zeros past an extent's end, so that the region maps whole in one MAP all the same, which each of
 * the files of the extent then slices: refused, each was a range of its own, a MAP each, and a
 * sandbox's cold read of the Linux tree over an attached generation took 27 s, 19.6 of them in
 * 91,072 MAPs, against a fresh VM's 5.7 s on the publisher's store, its 1,005 (GCE r940,
 * TASK-229). A piece past it is a range's of its own, as cladefs_dax_region_map refuses it (-EXDEV).
 */
static u64 cladefs_dax_region_size(const struct cladefs_dax_extent *e)
{
	return e->extent_size ?: CLADEFS_DAX_SIZE;
}

/*
 * The region mapping extent `e`'s, made, unmapped, if there is none: with a reference more, the
 * caller's. -EAGAIN if the window has no room for it now, reclaiming ranges for it if regions
 * share the ranges' space: the caller holds a file's DAX semaphore, and can't wait for room.
 * cladefs: in a space of their own, regions reclaim nothing: their callers map what they would have
 * in ranges' (TASK-91.10).
 */
static struct cladefs_dax_region *cladefs_dax_get_region(struct cladefs_dax *dax,
						   const struct cladefs_dax_extent *e)
{
	unsigned long pages = DIV_ROUND_UP(cladefs_dax_region_size(e), PAGE_SIZE);
	struct cladefs_dax_region *region, *new;
	long window_offset;

	spin_lock(&dax->regions_lock);
	region = cladefs_dax_find_region(dax, e->cell, e->sequence);
	if (region)
		region->refs++;
	spin_unlock(&dax->regions_lock);
	if (region)
		return region;

	new = kzalloc(sizeof(*new), GFP_NOFS);
	if (!new)
		return ERR_PTR(-ENOMEM);
	/* A region maps an extent at any offset: its space needs no alignment. */
	if (dax->region_pool != dax->pool)
		window_offset = cladefs_dax_take_region_window(dax, pages);
	else
		window_offset = cladefs_dax_alloc_window(dax, pages, false, true);
	if (window_offset < 0) {
		kfree(new);
		return ERR_PTR(window_offset);
	}
	new->cell = e->cell;
	new->sequence = e->sequence;
	new->window_offset = window_offset;
	new->pages = pages;
	new->refs = 1;
	mutex_init(&new->lock);
	spin_lock(&dax->regions_lock);
	/* Another may have made it meanwhile. */
	region = cladefs_dax_find_region(dax, e->cell, e->sequence);
	if (region)
		region->refs++;
	else
		hash_add(dax->regions, &new->node, cladefs_dax_region_key(e->cell, e->sequence));
	spin_unlock(&dax->regions_lock);
	if (!region) {
		atomic_long_inc(&dax->region_count);
		return new;
	}
	cladefs_dax_give_window(dax, window_offset, pages);
	kfree(new);
	return region;
}

/*
 * Maps `region`'s sealed extent as far as page `end` at least, read-only: all of it if it can, so
 * that the next slices need no MAP; else as far as `end`, as a sealed extent may end before what
 * was allocated of it, past what was written. -EXDEV if the region's space doesn't reach so far.
 */
static int cladefs_dax_region_map(struct cladefs_dax *dax, struct cladefs_dax_region *region,
			       unsigned long end)
{
	unsigned long from, to;
	int err = 0;

	mutex_lock(&region->lock);
	from = region->mapped;
	if (end <= from)
		goto out;
	if (end > region->pages) {
		err = -EXDEV;
		goto out;
	}
	to = region->pages;
	for (;;) {
		trace_cladefs_dax_map(CLADEFS_TRACE_MAP_REACH, 0,
				   region->window_offset + ((u64)from << PAGE_SHIFT),
				   (u64)(to - from) << PAGE_SHIFT, 1);
		err = cladefs_device_map(dax->dev, region->cell, region->sequence,
				      (u64)from << PAGE_SHIFT, (u64)(to - from) << PAGE_SHIFT,
				      region->window_offset + ((u64)from << PAGE_SHIFT), true);
		cladefs_dax_delay();
		atomic64_inc(&dax->maps[CLADEFS_DAX_REACH]);
		atomic64_add((u64)(to - from) << PAGE_SHIFT, &dax->map_bytes[CLADEFS_DAX_REACH]);
		if (!err || to == end)
			break;
		to = end;
	}
	/*
	 * An extent the device doesn't hold (-ENOENT) maps nothing: its callers read its pages as
	 * holes, which the region must not claim as mapped, or a slice of it would hand the guest
	 * window pages the host never mapped. One open still (-EPERM), which Fxfs had taken for
	 * sealed, is mapped by a range of its own, which seals it first.
	 */
	if (!err)
		WRITE_ONCE(region->mapped, to);
	else if (err == -EPERM)
		atomic64_inc(&dax->open_maps);
	else if (err != -ENOENT)
		pr_warn("cladefs: mapping pages %lu to %lu of extent %llx:%llu, of %lu pages, at %llu of the DAX window failed: %d\n",
			from, to, region->cell, region->sequence, region->pages,
			region->window_offset, err);
out:
	mutex_unlock(&region->lock);
	return err;
}

/*
 * Makes `inode`'s chunk `chunk` a slice of its extent's region, if its data in the file's `size`
 * bytes is one piece of a sealed extent the window maps whole, and returns it, kept, in its file's
 * ranges. NULL if it isn't such a piece, or the window has no room for its region now. With its
 * file's DAX semaphore held for writing.
 */
static struct cladefs_dax_range *cladefs_dax_new_slice(struct cladefs_dax *dax, struct inode *inode,
						 pgoff_t chunk, loff_t size)
{
	struct cladefs_dax_file *file = cladefs_dax_file(inode);
	loff_t start = (loff_t)chunk << CLADEFS_DAX_SHIFT;
	loff_t end = min_t(loff_t, start + CLADEFS_DAX_SIZE, round_up(size, PAGE_SIZE));
	struct cladefs_dax_region *region;
	struct cladefs_dax_range *range;
	struct cladefs_dax_extent e;
	unsigned long first, pages;
	ssize_t count;
	int err;

	lockdep_assert_held_write(&file->sem);

	if (end <= start || READ_ONCE(file->no_slices))
		return NULL;
	pages = (end - start) >> PAGE_SHIFT;
	count = cladefs_dax_sealed_extents(dax, cladefs_vfs_fs(inode->i_sb), inode->i_ino, start,
					end - start, &e, 1);
	cladefs_dax_delay();
	/* Holes, or more than one piece. */
	if (count == 0 || count == -E2BIG)
		return NULL;
	if (count < 0)
		return ERR_PTR(count);
	/* Nothing maps an open extent: a range of its own maps the chunk, sealing it first. */
	if (e.offset != start || e.len != end - start || cladefs_dax_region_size(&e) > CLADEFS_DAX_SIZE ||
	    !PAGE_ALIGNED(e.extent_offset) ||
	    e.access != CLADEFS_DAX_SEALED)
		return NULL;
	region = cladefs_dax_get_region(dax, &e);
	if (IS_ERR(region))
		return PTR_ERR(region) == -EAGAIN ? NULL : ERR_CAST(region);
	first = e.extent_offset >> PAGE_SHIFT;
	err = cladefs_dax_region_map(dax, region, first + pages);
	if (err)
		goto out;
	range = cladefs_dax_new_range(region->window_offset + ((u64)first << PAGE_SHIFT), pages,
				   region);
	if (IS_ERR(range)) {
		err = PTR_ERR(range);
		goto out;
	}
	range->file = file;
	range->chunk = chunk;
	atomic_set(&range->users, 1);
	/* Its pages are its region's, mapped. */
	cladefs_dax_pages_fill(range->state, 0, pages, false);
	err = xa_err(xa_store(&file->ranges, chunk, range, GFP_NOFS));
	if (err) {
		/* Its slice goes with it. */
		cladefs_dax_free_range(dax, range, 0);
		return ERR_PTR(err);
	}
	spin_lock(&dax->lock);
	list_add_tail(&range->list, &dax->used);
	spin_unlock(&dax->lock);
	return range;
out:
	cladefs_dax_put_region(dax, region);
	/*
	 * Its region can't reach it, its extent was never written, or is open still: a range of its
	 * own maps it, or the holes it is.
	 */
	return err == -EXDEV || err == -ENOENT || err == -EPERM ? NULL : ERR_PTR(err);
}

/*
 * Makes room for a fault that found none, holding the lock it held: reclaims a range, which it can
 * now, if one of its own inode's, or waits a moment for one released since `seen`.
 */
static void cladefs_dax_make_room(struct cladefs_dax *dax, long seen)
{
	if (!cladefs_dax_reclaim(dax))
		cladefs_dax_wait_released(dax, seen, 10);
}

/* How many pages the range of chunk `chunk` of a file `size` bytes long takes. */
static unsigned long cladefs_dax_range_pages(pgoff_t chunk, loff_t size)
{
	loff_t start = (loff_t)chunk << CLADEFS_DAX_SHIFT;

	if (size <= start)
		return 1;
	return min_t(unsigned long, CLADEFS_DAX_PAGES, DIV_ROUND_UP(size - start, PAGE_SIZE));
}

/*
 * How many pages a range of its own for chunk `chunk` of a file `size` bytes long takes: what the
 * file needs, rounded up to a power of two, so that a file growing a page at a time, as a log
 * appended to does, outgrows its range, and maps its data again, only as often as its size
 * doubles.
 */
static unsigned long cladefs_dax_range_capacity(pgoff_t chunk, loff_t size)
{
	return min_t(unsigned long, CLADEFS_DAX_PAGES,
		     roundup_pow_of_two(cladefs_dax_range_pages(chunk, size)));
}

static void cladefs_dax_use_range(struct cladefs_dax *dax, struct cladefs_dax_range *range)
{
	/* Its user keeps it linked and alive. Already last, this access cannot make it newer. */
	if (READ_ONCE(range->list.next) == &dax->used)
		return;
	spin_lock(&dax->lock);
	list_move_tail(&range->list, &dax->used);
	spin_unlock(&dax->lock);
}

/*
 * The range mapping `inode`'s chunk `chunk`, mapped first if it isn't, for a file at least `size`
 * bytes long, kept for an iomap: a slice of its extent's region if it can be and `slice` allows,
 * or a range of its own, every page of it mapped (cladefs_dax_map_pages). A write's chunk is a range
 * of its own (doc-13). For a fault (`fault`), -EAGAIN rather than wait for room in the window.
 */
static struct cladefs_dax_range *cladefs_dax_get_range(struct inode *inode, pgoff_t chunk, loff_t size,
						 bool fault, bool slice)
{
	struct cladefs_dax_file *file = cladefs_dax_file(inode);
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	struct cladefs_dax_range *range, *new;
	unsigned long pages;
	long mapped;
	int err;
	cladefs_nofs_scope();

	cladefs_nofs_down_read(&file->sem);
	range = xa_load(&file->ranges, chunk);
	if (range)
		atomic_inc(&range->users);
	cladefs_nofs_up_read(&file->sem);
	if (range)
		goto used;

	cladefs_nofs_down_write(&file->sem);
	range = xa_load(&file->ranges, chunk);
	if (range)
		atomic_inc(&range->users);
	else if (slice)
		range = cladefs_dax_new_slice(dax, inode, chunk, max3(i_size_read(inode), size,
								   READ_ONCE(file->growing)));
	cladefs_nofs_up_write(&file->sem);
	if (IS_ERR(range))
		return range;
	if (range)
		goto used;

	pages = cladefs_dax_range_capacity(chunk, max3(i_size_read(inode), size,
						    READ_ONCE(file->growing)));
	new = cladefs_dax_alloc_range(dax, pages, fault);
	if (IS_ERR(new))
		return new;
	cladefs_nofs_down_write(&file->sem);
	/* Another iomap may have mapped it meanwhile. */
	range = xa_load(&file->ranges, chunk);
	if (range) {
		atomic_inc(&range->users);
		cladefs_nofs_up_write(&file->sem);
		cladefs_dax_free_range(dax, new, 0);
		goto used;
	}
	new->file = file;
	new->chunk = chunk;
	new->no_pmd = !slice;
	atomic_set(&new->users, 1);
	err = xa_err(xa_store(&file->ranges, chunk, new, GFP_NOFS));
	if (!err) {
		spin_lock(&dax->lock);
		list_add_tail(&new->list, &dax->used);
		spin_unlock(&dax->lock);
		/* Every page of it, zeros past the file's end (doc-13); prefaulted, a read's. */
		mapped = cladefs_dax_map_pages(dax, cladefs_vfs_fs(inode->i_sb), new, 0, new->pages,
					    slice);
		err = mapped < 0 ? mapped : 0;
		/* Its one user, this, which no one else saw, the semaphore held throughout. */
		if (err) {
			atomic_dec(&new->users);
			cladefs_dax_unmap_range(dax, new);
		}
	} else {
		cladefs_dax_free_range(dax, new, 0);
	}
	cladefs_nofs_up_write(&file->sem);
	if (err)
		return ERR_PTR(err);
	range = new;
used:
	cladefs_dax_use_range(dax, range);
	return range;
}

static void cladefs_dax_put_range(struct cladefs_dax *dax, struct cladefs_dax_range *range)
{
	if (atomic_dec_and_test(&range->users))
		cladefs_dax_released(dax);
}

/*
 * Drops `inode`'s ranges of chunks `first` to `last` too small for the file to be `size` bytes
 * long, or all of them if `size` is negative, once nothing uses their pages: they are mapped again
 * as needed. A range's dirty pages are written back first: the UNMAP frees their private pages,
 * and the range maps again what Fxfs then has. With the inode's invalidate_lock held for writing.
 * cladefs: unless `pinned`, -EBUSY rather than wait for a page pinned (TASK-79.49).
 */
static int cladefs_dax_drop_pinned(struct inode *inode, pgoff_t first, pgoff_t last, loff_t size,
				bool pinned)
{
	struct cladefs_dax_file *file = cladefs_dax_file(inode);
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	struct cladefs_dax_range *range;
	unsigned long chunk;
	int err;
	cladefs_nofs_scope();

	for (;;) {
		long seen = atomic_long_read(&dax->released);
		bool busy = false, dirty = false;

		cladefs_nofs_down_write(&file->sem);
		xa_for_each_range(&file->ranges, chunk, range, first, last) {
			unsigned long need = size >= 0 ? cladefs_dax_range_pages(chunk, size) : 0;

			/*
			 * cladefs: a slice shrunk to a page-aligned size cleared its tail pages in
			 * `range->state` without unmapping its shared region (cladefs_dax_resize_end).
			 * Growing back into any unmapped page of a slice must drop the slice so a
			 * read never MAPs zeros over the shared region's window (board 2508, TASK-556).
			 */
			if (size >= 0 && range->pages >= need &&
			    (!range->region ||
			     cladefs_dax_pages_next_unmapped(range->state, 0, need) == need))
				continue;
			dirty = cladefs_dax_range_dirty(range);
			if (dirty || atomic_read(&range->users) || !cladefs_dax_release(inode, range)) {
				busy = true;
				break;
			}
			cladefs_dax_unmap_range(dax, range);
		}
		cladefs_nofs_up_write(&file->sem);
		if (!busy)
			return 0;
		if (dirty) {
			err = cladefs_dax_write_back(inode);
			if (err)
				return err;
			continue;
		}
		/* cladefs: a page pinned, which a fault in the kernel doesn't wait for. */
		if (!pinned && dax_layout_busy_page_range(inode->i_mapping,
							   (loff_t)first << CLADEFS_DAX_SHIFT,
							   ((loff_t)(last + 1) << CLADEFS_DAX_SHIFT) - 1))
			return -EBUSY;
		/* A fault in progress, or a page pinned: wait for it. */
		err = cladefs_dax_break_layouts(inode, (loff_t)first << CLADEFS_DAX_SHIFT);
		if (err)
			return err;
		if (cladefs_dax_wait_released(dax, seen, 10) < 0)
			return -EINTR;
	}
}

/* cladefs_dax_drop_pinned, waiting for pages pinned. */
static int cladefs_dax_drop(struct inode *inode, pgoff_t first, pgoff_t last, loff_t size)
{
	return cladefs_dax_drop_pinned(inode, first, last, size, true);
}

/*
 * Drops the ranges of `inode`'s file too small for it to be `size` bytes long: those of the chunks
 * its end moves past or into, which are mapped again as large as they need. With the inode's lock
 * and invalidate_lock held for writing.
 */
static int cladefs_dax_fit(struct inode *inode, loff_t size)
{
	loff_t old = i_size_read(inode);

	lockdep_assert_held_write(&inode->i_mapping->invalidate_lock);

	if (size <= old)
		return 0;
	return cladefs_dax_drop(inode, old >> CLADEFS_DAX_SHIFT, (size - 1) >> CLADEFS_DAX_SHIFT, size);
}

/*
 * Readies `inode`'s range of the chunk at `pos` for a write that returned -EXDEV:
 * drops and unmaps a slice (`range->region`) or a range too small to cover `pos`; for a range of
 * its own that still allows PMD mappings (`!range->no_pmd`), forbids future PMD faults and, if a
 * PMD entry was mapped (`has_pmd`), invalidates the chunk's DAX entries in place so its 2 MiB
 * compound folio splits back to order-0 folios without unmapping the chunk from the window
 * (board 14407). With the inode's invalidate_lock held for writing.
 * cladefs: unless `pinned`, -EBUSY rather than wait for pages pinned (TASK-79.49).
 */
static int cladefs_dax_prepare_write_range(struct inode *inode, loff_t pos, bool pinned)
{
	struct cladefs_dax_file *file = cladefs_dax_file(inode);
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	pgoff_t chunk = pos >> CLADEFS_DAX_SHIFT;
	struct cladefs_dax_range *range;
	int err;
	cladefs_nofs_scope();

	lockdep_assert_held_write(&inode->i_mapping->invalidate_lock);

	for (;;) {
		long seen = atomic_long_read(&dax->released);
		bool busy = false, dirty = false;

		cladefs_nofs_down_write(&file->sem);
		range = xa_load(&file->ranges, chunk);
		if (range) {
			loff_t start = cladefs_dax_chunk_start(range);
			bool drop = range->region ||
				    start + ((loff_t)range->pages << PAGE_SHIFT) <= pos;

			if (drop) {
				dirty = cladefs_dax_range_dirty(range);
				if (dirty || atomic_read(&range->users) ||
				    !cladefs_dax_release(inode, range))
					busy = true;
				else
					cladefs_dax_unmap_range(dax, range);
			} else if (!range->no_pmd) {
				if (!READ_ONCE(range->has_pmd)) {
					smp_store_release(&range->no_pmd, true);
				} else {
					dirty = cladefs_dax_range_dirty(range);
					if (dirty || atomic_read(&range->users) ||
					    !cladefs_dax_release(inode, range)) {
						busy = true;
					} else {
						WRITE_ONCE(range->has_pmd, false);
						smp_store_release(&range->no_pmd, true);
					}
				}
			}
		}
		cladefs_nofs_up_write(&file->sem);
		if (!busy)
			return 0;
		if (dirty) {
			err = cladefs_dax_write_back(inode);
			if (err)
				return err;
			continue;
		}
		if (!pinned && dax_layout_busy_page_range(inode->i_mapping,
							   (loff_t)chunk << CLADEFS_DAX_SHIFT,
							   ((loff_t)(chunk + 1) << CLADEFS_DAX_SHIFT) - 1))
			return -EBUSY;
		err = cladefs_dax_break_layouts(inode, (loff_t)chunk << CLADEFS_DAX_SHIFT);
		if (err)
			return err;
		if (cladefs_dax_wait_released(dax, seen, 10) < 0)
			return -EINTR;
	}
}

/*
 * Readies `inode`'s range of the chunk at `pos` after a write returned -EXDEV: dropping a slice or
 * undersized range, or splitting a PMD-mapped range back to order-0 folios. cladefs: unless
 * `pinned`, -EBUSY rather than wait for pages pinned (TASK-79.49).
 */
static int cladefs_dax_drop_slice(struct inode *inode, loff_t pos, bool pinned)
{
	int err;

	filemap_invalidate_lock(inode->i_mapping);
	err = cladefs_dax_prepare_write_range(inode, pos, pinned);
	filemap_invalidate_unlock(inode->i_mapping);
	return err;
}

/*
 * cladefs: drops the slice a fault can't use: 0 to try again, or what it returns, which is `ret` if
 * the slice stays. A fault from user mode holds no pins, so it waits for those others hold, as a
 * truncate does, the mapping lock held, and a signal sends it back to fault again. One from the
 * kernel may hold them itself, as get_user_pages does while it faults, and fails rather than wait
 * for itself (TASK-79.49).
 */
static vm_fault_t cladefs_dax_fault_drop_slice(struct vm_fault *vmf, vm_fault_t ret)
{
	bool user = vmf->flags & FAULT_FLAG_USER;
	int err = cladefs_dax_drop_slice(file_inode(vmf->vma->vm_file),
				      (loff_t)vmf->pgoff << PAGE_SHIFT, user);

	if (!err)
		return 0;
	return user && (err == -EINTR || err == -ERESTARTSYS) ? VM_FAULT_NOPAGE : ret;
}

/* Fills in `iomap` for a hole of `length` bytes. */
static void cladefs_dax_hole(struct iomap *iomap, loff_t length)
{
	iomap->type = IOMAP_HOLE;
	iomap->addr = IOMAP_NULL_ADDR;
	iomap->length = length;
}

/*
 * cladefs: maps those of `range`'s pages `first` to `last`, exclusive, that have no window page, as
 * a truncate's MAP of zeros that failed leaves the pages past its end (cladefs_dax_resize_end), which
 * a growth brings back into `inode`, so that every page of the file has one (cladefs_dax_map_pages):
 * 0 holding the file's semaphore shared, as an iomap does, or a -errno holding nothing of it.
 * Before any iomap of such a page, a read's as a write's: a read's hole has a fault map the
 * kernel's zero page, which nothing takes away, no write being new (IOMAP_F_NEW), so that a load
 * read zeros where read(2) read a write's bytes, and a write from the mapping copied them (guest/sim,
 * failures seed 1669). Prefaulted if `prefault`, a read's (cladefs_dax_map_pages).
 */
static int cladefs_dax_map_missing(struct inode *inode, struct cladefs_dax_range *range,
				unsigned long first, unsigned long last, bool prefault)
{
	struct cladefs_dax_file *file = range->file;
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	unsigned long page, to;
	long mapped;
	int err;

	for (;;) {
		cladefs_nofs_down_read(&file->sem);
		if (cladefs_dax_pages_next_unmapped(range->state, first, last) == last)
			return 0;
		cladefs_nofs_up_read(&file->sem);
		cladefs_nofs_down_write(&file->sem);
		err = 0;
		while (!err &&
		       (page = cladefs_dax_pages_next_unmapped(range->state, first, last)) < last) {
			to = cladefs_dax_pages_mapped_run(range->state, page, last);
			mapped = cladefs_dax_map_pages(dax, cladefs_vfs_fs(inode->i_sb), range, page, to,
						    prefault);
			err = mapped < 0 ? mapped : 0;
		}
		cladefs_nofs_up_write(&file->sem);
		if (err)
			return err;
	}
}

/*
 * Maps a read of up to `length` bytes at `pos`: a run of mapped pages, or of holes.
 */
static int cladefs_dax_iomap_read(struct inode *inode, loff_t pos, loff_t length, unsigned int flags,
			       struct iomap *iomap)
{
	struct cladefs_dax_file *file = cladefs_dax_file(inode);
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	struct cladefs_dax_range *range;
	unsigned long page, end_page;
	bool fault = flags & IOMAP_FAULT;
	loff_t start;
	bool mapped;
	u64 at = 0;
	int err;
	cladefs_nofs_scope();

	if (pos >= i_size_read(inode)) {
		cladefs_dax_hole(iomap, length);
		return 0;
	}
	/* A cache hit keeps the lookup's read lock through its iomap, rather than taking it twice. */
	cladefs_nofs_down_read(&file->sem);
	range = xa_load(&file->ranges, pos >> CLADEFS_DAX_SHIFT);
	if (range) {
		atomic_inc(&range->users);
		cladefs_dax_use_range(dax, range);
	} else {
		cladefs_nofs_up_read(&file->sem);
		range = cladefs_dax_get_range(inode, pos >> CLADEFS_DAX_SHIFT, 0, fault, true);
		if (IS_ERR(range))
			return PTR_ERR(range);
		cladefs_nofs_down_read(&file->sem);
	}
	start = cladefs_dax_chunk_start(range);
	page = (pos - start) >> PAGE_SHIFT;
	/* A range covers the file's chunk as its size grows (cladefs_dax_fit). */
	if (WARN_ON_ONCE(page >= range->pages)) {
		cladefs_nofs_up_read(&file->sem);
		cladefs_dax_put_range(dax, range);
		return -EIO;
	}
	/*
	 * cladefs: once a range has been written to (`no_pmd`), PMD read faults fall back to
	 * order-0 PTEs so no 2 MiB compound folio mixes private dirty pages with clean shared pages
	 * (board 14407).
	 */
	if (fault && length >= PMD_SIZE && smp_load_acquire(&range->no_pmd))
		length = PAGE_SIZE;
	/* cladefs: a page of the file with no window page is no hole: mapped first. */
	if (!cladefs_dax_pages_mapped(range->state, page)) {
		cladefs_nofs_up_read(&file->sem);
		err = cladefs_dax_map_missing(inode, range, page,
					   min_t(u64, range->pages,
						 page + DIV_ROUND_UP(offset_in_page(pos) + length,
								     PAGE_SIZE)),
					   true);
		if (err) {
			cladefs_dax_put_range(dax, range);
			return err;
		}
	}
	mapped = cladefs_dax_pages_mapped(range->state, page);
	end_page = cladefs_dax_pages_mapped_run(range->state, page, range->pages);
	if (mapped) {
		at = cladefs_dax_page_offset(range, page);
		end_page = min_t(unsigned long, end_page,
				 page + DIV_ROUND_UP(offset_in_page(pos) + length, PAGE_SIZE));
	}
	length = min_t(loff_t, length, start + ((loff_t)end_page << PAGE_SHIFT) - pos);
	if (!mapped) {
		cladefs_nofs_up_read(&file->sem);
		cladefs_dax_put_range(dax, range);
		cladefs_dax_hole(iomap, length);
		return 0;
	}
	/* cladefs: a page's place within the window, or its fault dies of SIGBUS (TASK-121). */
	WARN(at + length > dax->window_len,
	     "cladefs: ino %lu page %lu read at %llx, %llx bytes, past the window's %llx\n",
	     inode->i_ino, page, at, length, dax->window_len);
	iomap->type = IOMAP_MAPPED;
	iomap->addr = at + offset_in_page(pos);
	iomap->length = length;
	iomap->private = range;
	if (fault) {
		cladefs_dax_trace(dax, CLADEFS_DAX_TRACE_READ, inode->i_ino,
			       (start >> PAGE_SHIFT) + page, at, 0, length >> PAGE_SHIFT, 0);
		if (length >= PMD_SIZE && IS_ALIGNED(at, PMD_SIZE)) {
			WRITE_ONCE(range->has_pmd, true);
			atomic64_inc(&dax->huge_maps);
		}
		/*
		 * cladefs: a slice's page may be in other files' DAX entries too, or this one's at
		 * another offset, as blocks of a staging extent or a reflink's are: said so, as XFS
		 * says of its reflinks' reads, or the kernel takes the page for one no entry has
		 * (dax_associate_entry's WARN, xfstests generic/075, TASK-140).
		 */
		if (range->region)
			iomap->flags |= IOMAP_F_SHARED;
	}
	cladefs_nofs_up_read(&file->sem);
	return 0;
}

/*
 * cladefs: a writeback's pages of a range: the dirty set it swapped out, and those of it that Fxfs
 * had no memory to write, which it keeps (TASK-207).
 */
struct cladefs_dax_batch {
	struct cladefs_dax_range *range;
	unsigned long pages[BITS_TO_LONGS(CLADEFS_DAX_PAGES)];
	unsigned long kept[BITS_TO_LONGS(CLADEFS_DAX_PAGES)];
	/*
	 * cladefs: of `pages`, those a pin held once they were write-protected
	 * (cladefs_dax_protect).
	 */
	unsigned long pinned[BITS_TO_LONGS(CLADEFS_DAX_PAGES)];
};

/* cladefs: the pages one part of a writeback's run writes: `end - page` from `page` of a range's. */
struct cladefs_dax_run_part {
	size_t batch;
	unsigned long page, end;
};

/*
 * cladefs: the most pages one cladefs_write_many writes back: a megabyte, as one of the page cache's
 * writeback's (CLADEFS_IO_MAX, super.c).
 */
#define CLADEFS_DAX_WRITEBACK_RUN 256

/*
 * cladefs: has Fxfs write the `nr` parts `parts` of file `ino`, `pages` dirty pages, paid for with
 * their room; or, with no part, of pages past the file's end, releases it.
 */
static int cladefs_dax_write_run(struct cladefs *fs, u64 ino, const struct cladefs_write_part *parts,
			      size_t nr, unsigned long pages)
{
	if (nr)
		return cladefs_write_many(fs, ino, parts, nr, (u64)pages << PAGE_SHIFT);
	if (pages)
		cladefs_release_dirty(fs, ino, (u64)pages << PAGE_SHIFT);
	return 0;
}

/*
 * cladefs: what a run of a writeback's, its `n` parts' pages `run` of `batch`'s, came to as Fxfs
 * wrote it with `err`: an error whose pages are lost, as the page cache's are after an I/O error;
 * or 0, if it wrote them, or had no memory to, which keeps them, in their batches' `kept`, `*kept`
 * counting them, for a writeback later to write (TASK-207).
 */
static int cladefs_dax_ran(struct cladefs_dax_batch *batch, const struct cladefs_dax_run_part *run,
			size_t n, int err, unsigned long *kept)
{
	size_t i;

	if (err != -ENOMEM)
		return err;
	for (i = 0; i < n; i++) {
		bitmap_set(batch[run[i].batch].kept, run[i].page, run[i].end - run[i].page);
		*kept += run[i].end - run[i].page;
	}
	return 0;
}

/*
 * cladefs: has Fxfs write the `nr` ranges' pages `batch`, of `inode`, from the window, where stores
 * made them private, to new extents, as the page cache's writeback writes folios
 * (cladefs_vfs_writepages): CLADEFS_DAX_WRITEBACK_RUN pages a call at most, each with the room its
 * dirtying reserved, up to the file's end. Fxfs copies them into its transfer buffer, checksums
 * that, and commits after the bytes; a store meanwhile reaches the copy or not, its page dirty
 * again either way. `parts` and `run` have room for CLADEFS_DAX_WRITEBACK_RUN. The first error of a
 * run whose pages are lost, the rest written still; a run Fxfs had no memory for keeps its pages,
 * `*kept` of them (cladefs_dax_run_failed). With the file's writeback lock held, from the swap of its
 * dirty set to the commit, which rebinding and truncation wait for (doc-13).
 */
static int cladefs_dax_write_pages(struct cladefs_dax *dax, struct inode *inode,
				struct cladefs_dax_batch *batch, size_t nr,
				struct cladefs_write_part *parts, struct cladefs_dax_run_part *run_parts,
				unsigned long *kept)
{
	struct cladefs *fs = cladefs_vfs_fs(inode->i_sb);
	loff_t size = i_size_read(inode);
	unsigned long run = 0, page, end = 0;
	size_t i, n = 0;
	int err = 0, ran, id;

	lockdep_assert_held(&cladefs_dax_file(inode)->wb_lock);
	id = dax_read_lock();
	for (i = 0; i < nr; i++) {
		struct cladefs_dax_range *range = batch[i].range;
		loff_t start = cladefs_dax_chunk_start(range);

		for (page = find_first_bit(batch[i].pages, range->pages); page < range->pages;
		     page = find_next_bit(batch[i].pages, range->pages, end)) {
			loff_t pos = start + ((loff_t)page << PAGE_SHIFT);
			void *kaddr = NULL;
			long had;

			end = min(find_next_zero_bit(batch[i].pages, range->pages, page),
				  page + CLADEFS_DAX_WRITEBACK_RUN - run);
			had = dax_direct_access(dax->dax_dev,
						cladefs_dax_page_offset(range, page) >> PAGE_SHIFT,
						end - page, DAX_ACCESS, &kaddr, NULL);
			/* Past the file's end, as a write that copied nothing there leaves: nothing. */
			if (pos < size && had >= (long)(end - page)) {
				run_parts[n] = (struct cladefs_dax_run_part) {
					.batch = i, .page = page, .end = end,
				};
				parts[n++] = (struct cladefs_write_part) {
					.offset = pos,
					.data = kaddr,
					.len = min_t(loff_t, (loff_t)(end - page) << PAGE_SHIFT,
						     size - pos),
				};
			} else if (pos < size) {
				err = err ?: -EIO;
			}
			run += end - page;
			if (run == CLADEFS_DAX_WRITEBACK_RUN) {
				ran = cladefs_dax_write_run(fs, inode->i_ino, parts, n, run);
				/* Every run's kept, whatever came before. */
				ran = cladefs_dax_ran(batch, run_parts, n, ran, kept);
				err = err ?: ran;
				n = 0;
				run = 0;
			}
		}
	}
	ran = cladefs_dax_write_run(fs, inode->i_ino, parts, n, run);
	dax_read_unlock(id);
	ran = cladefs_dax_ran(batch, run_parts, n, ran, kept);
	return err ?: ran;
}

/*
 * cladefs: makes the user's mappings of the `nr` ranges' pages `batch`, a writeback's, read-only
 * and clean (mapping_wrprotect_range, as pfn_mkclean_range cleans a DAX entry's): a store to one
 * after this faults, and dirties its page again. Clean too: the kernel makes a shared mapping's PTE
 * writable with no fault where it is dirty (can_change_shared_pte_writable), as mprotect and a
 * userfaultfd's unprotect do, taking that the filesystem was told of it, which a writeback's page
 * it wrote back no longer is: write-protected alone (wp_shared_mapping_range), a store after an
 * mprotect toggle went into a page outside every dirty set, the host's copy, or the sealed extent a
 * rebind mapped (TASK-558). By the page tables, which see every PTE, not the DAX entries: a fault
 * past its iomap, which made its page dirty and its PTE writable, holds the page's entry until it
 * returns, after its iomap_end let the semaphore go and a writeback swapped the page out; and
 * dax_writeback_mapping_range, finding the entry locked, gives up on it rather than wait
 * (dax_writeback_one's get_next_unlocked_entry, mid-walk, says it gone). That PTE stayed writable:
 * a rebind mapped the page read-only under it, which the host refuses a store to, or a store
 * through it after the writeback's copy went in no writeback (guest/sim, seed 22). A run of pages
 * by its first page's frame: a range's pages are a run of the window's, which is one run of frames.
 *
 * And notes which pages a pin holds, then (`pinned`), which the writeback keeps dirty: a pin for
 * writing, short-term (an O_DIRECT read into the mapping), faulted its page dirty as it was taken,
 * and its stores come later, through the kernel's mapping or by DMA, with no fault, after the
 * writeback's copy may be (TASK-598). A pin taken after this faults, its PTE read-only, and
 * dirties its page again; one let go before this stored before the copy; one racing this,
 * get_user_pages' fast walk, finds the PTE changed under it and takes no pin, or is seen here.
 */
static void cladefs_dax_protect_run(struct cladefs_dax *dax, struct address_space *mapping,
				 struct cladefs_dax_batch *batch, unsigned long page,
				 unsigned long end)
{
	const struct cladefs_dax_range *range = batch->range;
	pgoff_t index = (cladefs_dax_chunk_start(range) >> PAGE_SHIFT) + page;
	pgoff_t window = cladefs_dax_page_offset(range, page) >> PAGE_SHIFT;
	unsigned long pfn, at;
	void *kaddr;

	/*
	 * A dirty page's frame: its part of the window has struct pages, a fault's. With none, the
	 * run unmapped, its next access a fault, and kept dirty, its pins unknown.
	 */
	if (WARN_ON_ONCE(dax_direct_access(dax->dax_dev, window, 1, DAX_ACCESS, &kaddr,
					   &pfn) < 1)) {
		unmap_mapping_pages(mapping, index, end - page, false);
		bitmap_set(batch->pinned, page, end - page);
		return;
	}
	mapping_wrprotect_range(mapping, index, pfn, end - page);
	for (at = page; at < end; at++) {
		if (folio_maybe_dma_pinned(page_folio(pfn_to_page(pfn + at - page))))
			__set_bit(at, batch->pinned);
	}
}

static void cladefs_dax_protect(struct cladefs_dax *dax, struct address_space *mapping,
			     struct cladefs_dax_batch *batch, size_t nr)
{
	int id = dax_read_lock();
	unsigned long page, end;
	size_t i;

	for (i = 0; i < nr; i++) {
		unsigned long pages = batch[i].range->pages;

		for (page = find_first_bit(batch[i].pages, pages); page < pages;
		     page = find_next_bit(batch[i].pages, pages, end)) {
			end = find_next_zero_bit(batch[i].pages, pages, page);
			cladefs_dax_protect_run(dax, mapping, &batch[i], page, end);
		}
	}
	dax_read_unlock(id);
}

/*
 * cladefs: a writeback's pages Fxfs had no memory to write, or a pin held (`batch`'s `kept`), dirty
 * again in their file's dirty set, their room reserved again, which the writeback took
 * (cladefs_write_many), what there is of it, so that a writeback later writes them (TASK-207,
 * TASK-598). Clean, as the page cache's after an I/O error, they were lost: the private pages read
 * as written, and a clone, which takes the store's blocks, a rebind or an eviction brought back the
 * blocks they replaced (GCE r699). With the writeback lock held, which keeps their ranges from
 * rebinding meanwhile. How many it made dirty: not those a truncate or an UNMAP took meanwhile, nor
 * those a store dirtied again.
 */
static unsigned long cladefs_dax_keep(struct inode *inode, struct cladefs_dax_batch *batch, size_t nr)
{
	struct cladefs_dax_file *file = cladefs_dax_file(inode);
	unsigned long made = 0;
	size_t i;
	cladefs_nofs_scope();

	cladefs_nofs_down_write(&file->sem);
	for (i = 0; i < nr; i++) {
		struct cladefs_dax_range *range = batch[i].range;
		unsigned long n = cladefs_dax_pages_redirty(range->state, batch[i].kept);

		if (n)
			xa_set_mark(&file->ranges, range->chunk, XA_MARK_0);
		made += n;
	}
	cladefs_dax_count(cladefs_dax_of(inode), file, 0, made);
	cladefs_nofs_up_write(&file->sem);
	if (!made)
		return 0;
	cladefs_reserve_dirty_what_fits(cladefs_vfs_fs(inode->i_sb), inode->i_ino,
				     (u64)made << PAGE_SHIFT);
	/* For the flusher's writepages, as cladefs_dax_dirty marks it. */
	__mark_inode_dirty(inode, I_DIRTY_PAGES);
	return made;
}

/*
 * cladefs: makes `batch`'s pages a pin held its `kept`, which cladefs_dax_keep makes dirty again,
 * so that the writeback after the pin's stores writes them (TASK-598): whether there were any.
 */
static bool cladefs_dax_take_pinned(struct cladefs_dax_batch *batch, size_t nr)
{
	bool any = false;
	size_t i;

	for (i = 0; i < nr; i++) {
		bitmap_copy(batch[i].kept, batch[i].pinned, CLADEFS_DAX_PAGES);
		any |= !bitmap_empty(batch[i].pinned, CLADEFS_DAX_PAGES);
	}
	return any;
}

/*
 * cladefs: the writeback transition (doc-13): writes back `inode`'s dirty pages, as fsync, msync,
 * sync, the flusher (writepages), the budget, an UNMAP of a range, a size's change and eviction
 * need, from dax.c's dirty set alone. Under its file's semaphore held for writing, so that no
 * write's iomap is between dirtying a page and its store or PTE, the dirty set is swapped for an
 * empty one: the writeback set, each page of it the VM's own on the host since the iomap that
 * dirtied it (cladefs_dax_dirty), so that its block may move under it. The writeback lock (`wb_lock`)
 * keeps the set from rebinding until Fxfs has committed it. Then what faults made writable is
 * write-protected (cladefs_dax_protect), so that a store after faults, and dirties its page again;
 * and the set's bytes are written to new extents, which Fxfs allocates now, committing after them
 * (cladefs_dax_write_pages). Swapped under the semaphore shared, a fault between its iomap and its PTE
 * left a writable PTE to a page the set took, which a rebind then lost the stores of. Fxfs takes a
 * file's size as its writeback writes its pages, as the page cache's. cladefs: pages Fxfs had no
 * memory to write stay dirty if `keep` (cladefs_dax_keep, TASK-207), the writeback failing ENOMEM;
 * an I/O error's are clean, the file's error set, as the page cache's.
 */
/* cladefs: how many of `file`'s ranges hold dirty pages: with its semaphore held, shared at least. */
static size_t cladefs_dax_dirty_ranges(struct cladefs_dax_file *file)
{
	struct cladefs_dax_range *range;
	unsigned long chunk;
	size_t dirty = 0;

	lockdep_assert_held(&file->sem);
	xa_for_each_marked(&file->ranges, chunk, range, XA_MARK_0)
		dirty++;
	return dirty;
}

/*
 * cladefs: swaps `range`'s dirty set out into `set`, its writeback's: how many pages it held. With
 * its file's semaphore held for writing, so that no iomap dirties a page of it meanwhile.
 */
static unsigned long cladefs_dax_range_swap(struct cladefs_dax_range *range, unsigned long *set)
{
	lockdep_assert_held_write(&range->file->sem);
	return cladefs_dax_pages_swap(range->state, set);
}

static int cladefs_dax_write_back_keeping(struct inode *inode, bool keep)
{
	struct cladefs_dax_file *file = cladefs_dax_file(inode);
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	struct cladefs_dax_batch *batch = NULL;
	struct cladefs_write_part *parts = NULL;
	struct cladefs_dax_run_part *run_parts = NULL;
	unsigned long kept = 0;
	struct cladefs_dax_range *range;
	unsigned long chunk;
	size_t nr = 0, most, now, i;
	long pages = 0, written = 0;
	bool traced = false;
	int err = 0;
	cladefs_nofs_scope();

	if (!IS_DAX(inode) || !file)
		return 0;
	/*
	 * Whatever is dirty now: a writeback under way, the budget's or the flusher's, swapped out
	 * pages dirtied before this began, which it holds the lock over until Fxfs has committed
	 * them, and an fsync returns only once they are durable. Returning at once, as no page was
	 * dirty, an fsync met by the worker's writeback sealed none of its pages (guest/sim, seed 962).
	 */
	cladefs_nofs_mutex_lock(&file->wb_lock);
	if (!atomic_long_read(&file->dirty_pages))
		goto out;
	traced = true;
	trace_cladefs_dax_writeback_start(file->ino);
	parts = kmalloc_array(CLADEFS_DAX_WRITEBACK_RUN, sizeof(*parts), GFP_NOFS);
	run_parts = kmalloc_array(CLADEFS_DAX_WRITEBACK_RUN, sizeof(*run_parts), GFP_NOFS);
	if (!parts || !run_parts) {
		err = -ENOMEM;
		goto out;
	}
	cladefs_nofs_down_read(&file->sem);
	most = cladefs_dax_dirty_ranges(file);
	cladefs_nofs_up_read(&file->sem);
	/*
	 * The batch, before the swap, as many as the semaphore held shared counted: a writeback that
	 * can't write leaves the set dirty. Counted again once it is held for writing, which keeps
	 * iomaps from dirtying more, and made larger if they did beside the count: one too small passed
	 * by the ranges last in the file, dirty before an fsync began, which returned 0 without them
	 * (guest/sim's file model, failures seed 62).
	 */
	for (;;) {
		if (!most)
			goto out;
		batch = kvmalloc_array(most, sizeof(*batch), GFP_KERNEL);
		if (!batch) {
			err = -ENOMEM;
			goto out;
		}
		/* Between the count and the swap, where iomaps may dirty more (pause points). */
		cladefs_dax_pause_file(CLADEFS_DAX_STEP_COUNTED, inode->i_ino);
		cladefs_nofs_down_write(&file->sem);
		now = cladefs_dax_dirty_ranges(file);
		if (now && now <= most)
			break;
		cladefs_nofs_up_write(&file->sem);
		kvfree(batch);
		batch = NULL;
		most = now;
	}
	xa_for_each_marked(&file->ranges, chunk, range, XA_MARK_0) {
		xa_clear_mark(&file->ranges, chunk, XA_MARK_0);
		batch[nr].range = range;
		/* Kept, mapped, while the writeback reads it: no reclaim, no drop. */
		atomic_inc(&range->users);
		pages += cladefs_dax_range_swap(range, batch[nr].pages);
		bitmap_zero(batch[nr].kept, CLADEFS_DAX_PAGES);
		bitmap_zero(batch[nr].pinned, CLADEFS_DAX_PAGES);
		nr++;
	}
	cladefs_dax_count(dax, file, 0, -pages);
	cladefs_nofs_up_write(&file->sem);
	cladefs_dax_protect(dax, inode->i_mapping, batch, nr);
	/* Before it reads its ranges' window pages, which nothing may free (pause points). */
	cladefs_dax_pause_file(CLADEFS_DAX_STEP_WRITEBACK, inode->i_ino);
	err = cladefs_dax_write_pages(dax, inode, batch, nr, parts, run_parts, &kept);
	if (kept && !keep) {
		err = err ?: -ENOMEM;
		kept = 0;
	} else if (kept) {
		kept = cladefs_dax_keep(inode, batch, nr);
	}
	/* Written, and dirty again, those a pin held (cladefs_dax_protect), but by an eviction. */
	if (keep && cladefs_dax_take_pinned(batch, nr))
		cladefs_dax_keep(inode, batch, nr);
	for (i = 0; i < nr; i++)
		cladefs_dax_put_range(dax, batch[i].range);
	atomic64_inc(&dax->writebacks);
	written = pages - kept;
	atomic64_add(written, &dax->written);
	/*
	 * As the page cache's writeback: what an I/O error stopped is clean, the error the file's
	 * (fsync's, syncfs's). What Fxfs had no memory for is dirty still (TASK-207), and this
	 * writeback says so, ENOMEM, the file's error not set: the next writes it.
	 */
	if (err)
		mapping_set_error(inode->i_mapping, err);
	if (kept)
		err = err ?: -ENOMEM;
out:
	if (traced)
		trace_cladefs_dax_writeback_end(file->ino, err ?: written);
	cladefs_nofs_mutex_unlock(&file->wb_lock);
	kfree(run_parts);
	kfree(parts);
	kvfree(batch);
	return err;
}

int cladefs_dax_write_back(struct inode *inode)
{
	return cladefs_dax_write_back_keeping(inode, true);
}


/*
 * cladefs: the rebinding transition (doc-13): rebinds `file`'s clean private pages, those in no dirty
 * set, nor in a writeback's under way, which the writeback lock waits for, each MAPped to the
 * sealed extent's page holding its bytes, as Fxfs has them, or to zeros where it has none: the
 * same bytes at the same address, the host's private page freed, nothing in the guest changed, and
 * the page private no more (cladefs_dax_mapped). Under the file's semaphore held for writing, which
 * sees each with no writable PTE (the fault that made one dirtied its page, which its writeback
 * write-protected) and no copy into it under way (invariant 3). A page whose extent is open yet,
 * its writeback's barrier not come, stays private. Range by range, until no more than `target`
 * pages are private: the writeback lock and the semaphore taken for each range, which held through
 * a file's every range kept its readers, writers and fsyncs waiting through every MAP of them
 * (TASK-164.10). How many it rebound, and in `*open` how many it left private, their extents open.
 */
static long cladefs_dax_rebind_ranges(struct cladefs_dax *dax, struct cladefs_dax_file *file, long target,
				   unsigned long *open)
{
	struct cladefs_dax_volume *volume;
	struct cladefs_dax_range *range;
	unsigned long chunk = 0;
	long rebound = 0, done;
	cladefs_nofs_scope();

	while (atomic_long_read(&file->private_pages) && atomic_long_read(&dax->private) > target) {
		cladefs_nofs_mutex_lock(&file->wb_lock);
		cladefs_nofs_down_write(&file->sem);
		/* A volume going (cladefs_dax_remove_volume) stops the worker first. */
		volume = READ_ONCE(file->volume);
		range = volume ? xa_find(&file->ranges, &chunk, ULONG_MAX, XA_PRESENT) : NULL;
		if (range) {
			done = cladefs_dax_rebind_range(dax, volume->fs, range, open);
			if (done < 0)
				pr_warn_ratelimited("cladefs: rebinding ino %llu chunk %lu: %ld\n",
						    file->ino, chunk, done);
			else
				rebound += done;
		}
		cladefs_nofs_up_write(&file->sem);
		cladefs_nofs_mutex_unlock(&file->wb_lock);
		if (!range || chunk == ULONG_MAX)
			break;
		chunk++;
		cond_resched();
	}
	return rebound;
}

/*
 * cladefs: rebinds `file`'s clean private pages (cladefs_dax_rebind_ranges) until no more than `target`
 * are private; and, if it left some private, their extents open, with more private than that still,
 * seals, and rebinds again. Writeback outside the budget's (a growth's or a truncate's, a write's
 * that stopped short, a drop's, the flusher's) seals nothing after it: what it wrote stays open
 * until the next fsync or sync seals it, and the worker sealed only what it wrote back itself. With
 * nothing dirty, as after interleave seed 215500's write stopped short, every round rebound none,
 * and a write's or a punch's wait for room failed ENOMEM (TASK-283). How many it rebound.
 */
static long cladefs_dax_rebind(struct cladefs_dax *dax, struct cladefs_dax_file *file, long target,
			       bool seal)
{
	struct cladefs_dax_volume *volume;
	unsigned long open = 0;
	long rebound;
	int err;

	trace_cladefs_dax_rebind_start(file->ino);
	rebound = cladefs_dax_rebind_ranges(dax, file, target, &open);
	/* A volume going (cladefs_dax_remove_volume) stops the worker first. */
	volume = READ_ONCE(file->volume);
	/* Never fsync while frozen (`!seal`): hold_commits and the closed device gate deadlock (board 12113). */
	if (seal && open && volume && atomic_long_read(&dax->private) > target) {
		atomic64_inc(&dax->seals_made);
		err = cladefs_nofs(cladefs_fsync(volume->fs));
		if (err)
			pr_warn_ratelimited("cladefs: sealing ino %llu's private pages: %d\n", file->ino,
					    err);
		else
			rebound += cladefs_dax_rebind_ranges(dax, file, target, &open);
	}
	atomic64_add(rebound, &dax->rebound);
	trace_cladefs_dax_rebind_end(file->ino, rebound);
	return rebound;
}

/*
 * cladefs: references to every volume's files with dirty or private pages, in `*files`, which the
 * caller frees, putting each: how many, or -ENOMEM without the memory for them, when a sync
 * mustn't say it wrote them (guest/sim's sweep: it returned 0, and the files' writes were lost).
 */
static ssize_t cladefs_dax_busy_files(struct cladefs_dax *dax, struct cladefs_dax_file ***files)
{
	struct cladefs_dax_volume *volume;
	struct cladefs_dax_file *file;
	size_t nr = 0, most = 0;
	unsigned long ino;

	*files = NULL;
	mutex_lock(&dax->files_lock);
	/* files_lock fixes membership, but a clean file can become busy under it. */
	cladefs_dax_for_each_file(dax, volume, ino, file)
		most++;
	if (most)
		*files = cladefs_nofs(kvmalloc_array(most, sizeof(**files), GFP_KERNEL));
	if (*files) {
		cladefs_dax_for_each_file(dax, volume, ino, file) {
			if (atomic_long_read(&file->dirty_pages) ||
			    atomic_long_read(&file->private_pages)) {
				refcount_inc(&file->refs);
				(*files)[nr++] = file;
			}
		}
	}
	mutex_unlock(&dax->files_lock);
	return most && !*files ? -ENOMEM : nr;
}

/* cladefs: the inode read in for `file`, with a reference, if there is one. */
static struct inode *cladefs_dax_inode_of(struct cladefs_dax *dax, struct cladefs_dax_file *file)
{
	struct inode *inode = NULL;

	mutex_lock(&dax->files_lock);
	if (file->inode)
		inode = igrab(file->inode);
	mutex_unlock(&dax->files_lock);
	return inode;
}

/*
 * cladefs: orders files by when their pages were last made dirty or private (`dirtied`), the
 * longest ago first: the budget rebinds those first, where it went by inode number and rebound a
 * pgbench's log file, being written, its pages made private again at its next write (TASK-164.10).
 */
static int cladefs_dax_dirtied_before(const void *a, const void *b)
{
	unsigned long x = READ_ONCE((*(struct cladefs_dax_file *const *)a)->dirtied);
	unsigned long y = READ_ONCE((*(struct cladefs_dax_file *const *)b)->dirtied);

	return time_before(x, y) ? -1 : time_after(x, y) ? 1 : 0;
}

/*
 * cladefs: relieves the private pages (doc-13's budget): writes back every file with dirty pages,
 * if `write`, not while frozen; seals what that wrote, for its pages' extents to map; and rebinds
 * clean private pages, file by file, until no more than `target` are private.
 */
static void cladefs_dax_relieve(struct cladefs_dax *dax, long target, bool write)
{
	struct cladefs_dax_file **files;
	ssize_t nr = cladefs_dax_busy_files(dax, &files), i;
	struct cladefs *wrote = NULL;
	int err;

	/* Without the memory, the next round relieves them. */
	if (nr < 0)
		return;

	for (i = 0; write && i < nr; i++) {
		struct cladefs_dax_volume *volume = READ_ONCE(files[i]->volume);
		struct inode *inode;

		if (!volume || !atomic_long_read(&files[i]->dirty_pages))
			continue;
		inode = cladefs_dax_inode_of(dax, files[i]);
		if (!inode)
			continue;
		err = cladefs_dax_write_back(inode);
		if (err)
			pr_warn_ratelimited("cladefs: writing back ino %lu's private pages: %d\n",
					    inode->i_ino, err);
		iput(inode);
		wrote = volume->fs;
	}
	/* The barrier after them, which the journal's flush makes, seals them. */
	if (wrote) {
		atomic64_inc(&dax->seals_made);
		err = cladefs_nofs(cladefs_fsync(wrote));
		if (err)
			pr_warn_ratelimited("cladefs: sealing what the budget wrote back: %d\n", err);
	}
	/* The files dirtied longest ago first, which are least likely to be written again soon. */
	sort(files, nr, sizeof(*files), cladefs_dax_dirtied_before, NULL);
	for (i = 0; i < nr; i++) {
		if (atomic_long_read(&dax->private) > target)
			cladefs_dax_rebind(dax, files[i], target, write);
		cladefs_dax_put_file(dax, files[i]);
	}
	kvfree(files);
}

/*
 * cladefs: the seal count at which what was written before now is sealed: that of the end of the
 * first barrier begun after now. One in progress (odd) may have been sent before: the next.
 */
static u64 cladefs_dax_seal_target(struct cladefs_dax *dax)
{
	u64 seals = cladefs_device_seals(dax->dev);

	return seals + 2 + (seals & 1);
}

/* cladefs: forgets the page-cache inodes of the volumes' `unsealed` whose barrier has come. */
static void cladefs_dax_prune(struct cladefs_dax *dax)
{
	u64 seals = cladefs_device_seals(dax->dev);
	struct cladefs_dax_volume *volume;
	unsigned long ino;
	void *entry;

	if (!atomic_long_read(&dax->unsealed))
		return;
	mutex_lock(&dax->files_lock);
	list_for_each_entry(volume, &dax->volumes, list) {
		xa_for_each(&volume->unsealed, ino, entry) {
			if (seals >= xa_to_value(entry) &&
			    xa_cmpxchg(&volume->unsealed, ino, entry, NULL, GFP_NOFS) == entry)
				atomic_long_dec(&dax->unsealed);
		}
	}
	mutex_unlock(&dax->files_lock);
}

/*
 * cladefs: the budget's worker (doc-13): past seven eighths of private_max, it writes back dirty
 * files and rebinds clean private pages, to three quarters; asked to (`clean`, as the window runs
 * out of room for ranges), it writes back whatever is dirty; asked to rebind all
 * (cladefs.dax_rebind_now, cladefs.dax_rebind_every), as many as it can. Then it prunes the page-cache
 * inodes sealed since, says which asks to rebind all it served, those made before it began, and
 * wakes those the budget held up. The window's reclaim learns of ranges its writeback cleaned as
 * it lets go of them (cladefs_dax_put_range), not of a round: woken by every round, which each wait
 * for room asked for, a write that could never have room, its window's aligned room lost to UNMAPs
 * that failed, found something let go of each time, and waited for ever (guest/sim's sweep,
 * read_pressure).
 */
static void cladefs_dax_work(struct work_struct *work)
{
	struct cladefs_dax *dax = container_of(work, struct cladefs_dax, work);
	unsigned int every = cladefs_dax_rebind_every_ms();
	s64 asked = atomic64_read(&dax->rebinds_asked);
	bool clean = atomic_cmpxchg(&dax->clean, 1, 0) == 1;
	bool all = asked != atomic64_read(&dax->rebinds_done) || every;
	long max = dax->private_max;

	if (clean || all || atomic_long_read(&dax->private) > max - max / 8)
		cladefs_dax_relieve(dax, all ? 0 : max - max / 4,
				    !cladefs_is_frozen(dax->filesystem));
	cladefs_dax_prune(dax);
	atomic64_set(&dax->rebinds_done, asked);
	atomic64_inc(&dax->rounds);
	wake_up_all(&dax->budget_wait);
	if (every && !READ_ONCE(dax->stops)) {
		msleep(every);
		cladefs_dax_kick(dax, false);
	}
}

/*
 * cladefs: counts `pages` more pages private, as the iomap that dirties them has the host make them
 * so, if private_max has room for them; else none, and false, for the iomap to wait for the worker
 * (doc-13's budget). Past seven eighths of it, the worker writes back and rebinds.
 */
static bool cladefs_dax_take_private(struct cladefs_dax *dax, long pages)
{
	long now = atomic_long_add_return(pages, &dax->private);

	if (now > dax->private_max) {
		atomic_long_sub(pages, &dax->private);
		atomic64_inc(&dax->throttled);
		cladefs_dax_kick(dax, false);
		return false;
	}
	if (now > dax->private_max - dax->private_max / 8)
		cladefs_dax_kick(dax, false);
	return true;
}

/*
 * cladefs: waits, holding nothing of the file's, for a round of the worker's since a write's iomap
 * found no room among the private pages (-ENOBUFS), as balance_dirty_pages waits after a fault: 0
 * to try again, -EINTR if killed, or -ENOMEM once CLADEFS_DAX_NO_ROOM_WAITS rounds or waits of 10 ms
 * made none, as for a range.
 */
static int cladefs_dax_throttle(struct cladefs_dax *dax, unsigned int *waits)
{
	s64 round = atomic64_read(&dax->rounds);
	long waited;

	cladefs_dax_kick(dax, false);
	waited = wait_event_killable_timeout(dax->budget_wait,
					     atomic64_read(&dax->rounds) != round,
					     msecs_to_jiffies(10));
	if (waited < 0)
		return -EINTR;
	if (++*waits > CLADEFS_DAX_NO_ROOM_WAITS)
		return -ENOMEM;
	return 0;
}

/*
 * cladefs: a PRIVATE of `pages` pages at `offset` of the window failed with `err`: the `counted`
 * pages counted private for it given back, and the error its write fails with: ENOMEM as ENOMEM,
 * the guest's request's memory not found, not EIO (TASK-44.15); the host's refusal is EIO already
 * (cladefs_device_private).
 */
static int cladefs_dax_private_failed(struct cladefs_dax *dax, u64 offset, unsigned long pages,
				   long counted, int err)
{
	pr_warn_ratelimited("cladefs: making %lu pages at %llx of the window private: %d\n", pages,
			    offset, err);
	atomic_long_sub(counted, &dax->private);
	wake_up_all(&dax->budget_wait);
	return err == -ENOMEM ? err : -EIO;
}

/*
 * cladefs: the dirtying transition (doc-13), the one way a page joins its file's dirty set:
 * `range`'s `pages` pages from `first`, a write's (a fault's, a write(2)'s or zeroing's), about to
 * be stored to. Each not dirty yet gets room in Fxfs for its writeback (a fault without it fails
 * SIGBUS, a write(2) ENOSPC, as the page cache's do; zeroing, a truncate's or a punch's, takes
 * what there is). Each not private yet is counted private, within private_max, else -ENOBUFS, for
 * the caller to wait for the budget's worker, holding nothing (cladefs_dax_throttle); and the host
 * makes them the VM's own now (PRIVATE), holding what they held, before they are dirty: so a dirty
 * page is private, whenever its store lands. A fault's store lands after its iomap_end, maybe after
 * a writeback has taken its page, moved its block, and had Fxfs free the old one: the host copying
 * the page at the store, as it would, copied the zeros the window reads there then (guest/sim,
 * seed 2802 at 1,000 operations). A failed PRIVATE fails the write, -EIO, or -ENOMEM if the guest
 * found no memory for its request, nothing dirty, the room given back. With the
 * file's semaphore held shared, which the caller holds until the store or the copy is done
 * (cladefs_dax_iomap_end): no writeback swaps the page out, nor a rebind maps over it, meanwhile.
 *
 * A PRIVATE takes `ahead` pages more, after them, private and clean, if there is room for them
 * below where the budget's worker begins (cladefs_dax_take_private), and the write's own pages need
 * it: those the stores following on from the write's, a log's, reach next (cladefs_dax_iomap_write),
 * which take no round trip then (TASK-164.10).
 */
static int cladefs_dax_dirty(struct inode *inode, struct cladefs_dax_range *range, unsigned long first,
			  unsigned long pages, bool zero, unsigned long ahead)
{
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	struct cladefs *fs = cladefs_vfs_fs(inode->i_sb);
	struct cladefs_dax_change would, made;
	long fresh, newly, beyond = 0, ahead_made = 0;
	int err = 0;

	lockdep_assert_held_read(&range->file->sem);
	would = cladefs_dax_pages_would_dirty(range->state, first, pages);
	fresh = would.dirty;
	newly = would.private;
	if (fresh && zero) {
		cladefs_reserve_dirty_what_fits(fs, inode->i_ino, (u64)fresh << PAGE_SHIFT);
	} else if (fresh) {
		err = cladefs_reserve_dirty(fs, inode->i_ino, (u64)fresh << PAGE_SHIFT);
		if (err)
			return err;
	}
	if (newly && ahead) {
		beyond = cladefs_dax_pages_would_dirty(range->state, first + pages, ahead).private;
		if (atomic_long_read(&dax->private) + newly + beyond >
		    dax->private_max - dax->private_max / 8)
			ahead = beyond = 0;
	} else {
		ahead = 0;
	}
	if (newly && !cladefs_dax_take_private(dax, newly + beyond)) {
		err = -ENOBUFS;
		goto unreserve;
	}
	if (newly) {
		/* The pages private already, those others' iomaps made so meanwhile too, stay so. */
		err = cladefs_device_private(dax->dev, cladefs_dax_page_offset(range, first),
					  (u64)(pages + ahead) << PAGE_SHIFT);
		atomic64_inc(&dax->privates);
		if (err) {
			err = cladefs_dax_private_failed(dax, cladefs_dax_page_offset(range, first),
						      pages + ahead, newly + beyond, err);
			goto unreserve;
		}
		WRITE_ONCE(range->file->dirtied, jiffies);
	}
	made = cladefs_dax_pages_dirty(range->state, first, pages);
	if (ahead) {
		ahead_made = cladefs_dax_pages_private_ahead(range->state, first + pages, ahead);
		atomic64_add(ahead_made, &dax->private_ahead);
	}
	/* Others' iomaps, holding the semaphore shared too, may have counted some meanwhile. */
	if (newly + beyond > made.private + ahead_made)
		atomic_long_sub(newly + beyond - made.private - ahead_made, &dax->private);
	atomic_long_add(made.private + ahead_made, &range->file->private_pages);
	if (fresh > made.dirty)
		cladefs_release_dirty(fs, inode->i_ino, (u64)(fresh - made.dirty) << PAGE_SHIFT);
	if (made.dirty) {
		xa_set_mark(&range->file->ranges, range->chunk, XA_MARK_0);
		atomic_long_add(made.dirty, &dax->dirty);
		atomic_long_add(made.dirty, &range->file->dirty_pages);
		WRITE_ONCE(range->file->dirtied, jiffies);
		/* For the flusher's writepages: a write(2)'s pages tag nothing in the mapping. */
		__mark_inode_dirty(inode, I_DIRTY_PAGES);
	}
	return 0;
unreserve:
	if (fresh)
		cladefs_release_dirty(fs, inode->i_ino, (u64)fresh << PAGE_SHIFT);
	return err;
}

/*
 * cladefs: how many pages from `range`'s page `next`, the first after a write's iomap's, a PRIVATE
 * for the iomap makes private ahead of it (cladefs_dax_dirty): a write(2) following on from the file's
 * last, whose iomap reaches its end, `write_end`, takes as many as the file's run of such writes
 * says (cladefs_dax_write_follows), each mapped and written, in the range; none for a fault's, or a
 * zeroing's. A forked pgbench's log, a file of its base's, took a PRIVATE for every write, 62,146 a
 * minute, three pages each, each round trip on the way of the log's flush (TASK-164.10, GCE r649).
 */
static unsigned long cladefs_dax_write_ahead(struct inode *inode, struct cladefs_dax_range *range,
					  loff_t write_end, unsigned long next, bool not_write)
{
	unsigned long ahead = READ_ONCE(range->file->ahead), last;
	loff_t start = cladefs_dax_chunk_start(range);

	if (not_write || !ahead || start + ((loff_t)next << PAGE_SHIFT) < write_end ||
	    next >= range->pages)
		return 0;
	lockdep_assert_held_write(&inode->i_rwsem);
	last = min(range->pages, next + ahead);
	return cladefs_dax_pages_aheadable_run(range->state, next, last) - next;
}

/*
 * cladefs: maps a write of up to `length` bytes at `pos`, a fault's, a write(2)'s or zeroing's, at its
 * pages' own places in the window (doc-13), made dirty and private (cladefs_dax_dirty): no MAP, no
 * transaction, nothing moved. Returns holding the file's semaphore shared until
 * cladefs_dax_iomap_end: no writeback swaps the dirty set out, nor rebinding maps a page over, while
 * the store or the copy goes on. A slice's chunk, whose pages its region's other slices map, or a
 * range too small, is dropped (-EXDEV), and mapped again as a range of its own, as large as the
 * write needs. Zeroing leaves holes, and pages of zeros never written, be. A write past the file's
 * end grows it as it ends (cladefs_dax_iomap_end), unless it is a fault's.
 */
static int cladefs_dax_iomap_write(struct inode *inode, loff_t pos, loff_t length,
				unsigned int flags, struct iomap *iomap)
{
	struct cladefs_dax_file *file = cladefs_dax_file(inode);
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	bool fault = flags & IOMAP_FAULT, zero = flags & IOMAP_ZERO;
	loff_t start, end = min_t(loff_t, pos + length, round_up(pos + 1, CLADEFS_DAX_SIZE));
	unsigned long first, pages, page;
	struct cladefs_dax_range *range;
	int err;

	range = cladefs_dax_get_range(inode, pos >> CLADEFS_DAX_SHIFT, fault || zero ? 0 : end, fault,
				   false);
	if (IS_ERR(range))
		return PTR_ERR(range);
	start = cladefs_dax_chunk_start(range);
	if (range->region || !smp_load_acquire(&range->no_pmd) ||
	    start + ((loff_t)range->pages << PAGE_SHIFT) <= pos) {
		cladefs_dax_put_range(dax, range);
		return -EXDEV;
	}
	first = (pos - start) >> PAGE_SHIFT;
	end = min_t(loff_t, end, start + ((loff_t)min(range->pages, first + dax->iomap_pages) <<
					  PAGE_SHIFT));
	pages = DIV_ROUND_UP(end - start, PAGE_SIZE) - first;
	err = cladefs_dax_map_missing(inode, range, first, first + pages, false);
	if (err)
		goto put;
	if (zero) {
		bool hole = cladefs_dax_pages_unwritten(range->state, first);

		page = cladefs_dax_pages_unwritten_run(range->state, first, first + pages);
		pages = page - first;
		end = min_t(loff_t, end, start + ((loff_t)page << PAGE_SHIFT));
		if (hole) {
			cladefs_nofs_up_read(&file->sem);
			cladefs_dax_put_range(dax, range);
			cladefs_dax_hole(iomap, end - pos);
			return 0;
		}
	}
	err = cladefs_dax_dirty(inode, range, first, pages, zero,
			     cladefs_dax_write_ahead(inode, range, pos + length, first + pages,
						  fault || zero));
	if (err)
		goto unlock;
	iomap->type = IOMAP_MAPPED;
	iomap->addr = cladefs_dax_page_offset(range, first) + offset_in_page(pos);
	iomap->length = end - pos;
	iomap->private = range;
	/* cladefs: a page's place within the window, or its fault dies of SIGBUS (TASK-121). */
	WARN(iomap->addr >= dax->window_len,
	     "cladefs: ino %lu page %lu written at %llx, past the window's %llx\n",
	     inode->i_ino, first, iomap->addr, dax->window_len);
	cladefs_dax_trace(dax, fault ? CLADEFS_DAX_TRACE_FAULT : CLADEFS_DAX_TRACE_WRITE, inode->i_ino,
		       (start >> PAGE_SHIFT) + first, cladefs_dax_page_offset(range, first), 0, pages, 0);
	/* Before the copy, its pages dirty, holding the semaphore shared (pause points). */
	if (!fault && !zero)
		cladefs_dax_pause_file(CLADEFS_DAX_STEP_WRITE_MAPPED, inode->i_ino);
	return 0;
unlock:
	cladefs_nofs_up_read(&file->sem);
put:
	cladefs_dax_put_range(dax, range);
	return err;
}

static int cladefs_dax_iomap_begin(struct inode *inode, loff_t pos, loff_t length,
				unsigned int flags, struct iomap *iomap, struct iomap *srcmap)
{
	u64 began;
	int err;

	/*
	 * cladefs: an iomap a page at most for a write of a file in pieces: a write of many, each with
	 * its tree lookups, held its CPU for 22 s, no one else getting it, without preemption
	 * (xfstests generic/111, TASK-140).
	 */
	cond_resched();
	if (WARN_ON_ONCE(flags & IOMAP_REPORT))
		return -EIO;
	iomap->offset = pos;
	iomap->flags = 0;
	iomap->bdev = NULL;
	iomap->dax_dev = cladefs_dax_of(inode)->dax_dev;
	if (flags & (IOMAP_WRITE | IOMAP_ZERO)) {
		/*
		 * cladefs: its pages made private and dirty, nothing moved (doc-13). A write(2)'s timed,
		 * and no other iomap's: a read's two clock reads, for nothing, were 1-2% of a 91k-file
		 * tree's warm reread (GCE r901, r902, TASK-230).
		 */
		if (flags & (IOMAP_FAULT | IOMAP_ZERO))
			return cladefs_dax_iomap_write(inode, pos, length, flags, iomap);
		began = ktime_get_ns();
		err = cladefs_dax_iomap_write(inode, pos, length, flags, iomap);
		atomic64_add(ktime_get_ns() - began, &cladefs_dax_of(inode)->write_begin_ns);
		return err;
	}
	err = cladefs_dax_iomap_read(inode, pos, length, flags, iomap);
	/* cladefs: between the iomap and the read of what it maps, which its range keeps mapped. */
	if (!err) {
		cladefs_dax_pause_file(CLADEFS_DAX_STEP_READ_MAPPED, inode->i_ino);
		cladefs_dax_delay();
	}
	return err;
}

/*
 * A write's iomap held the file's semaphore shared, which goes now its store or copy is done
 * (cladefs_dax_iomap_write). A write(2) past the file's end grows it as it ends, by what it wrote, not
 * as it begins: a reader finding the file longer reads what the write wrote there, never the zeros
 * before it (TASK-133), as ext4's and XFS's DAX writes grow theirs.
 */
static int cladefs_dax_iomap_end(struct inode *inode, loff_t pos, loff_t length, ssize_t written,
			      unsigned int flags, struct iomap *iomap)
{
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	struct cladefs_dax_file *file = cladefs_dax_file(inode);
	bool written_by_write = (flags & IOMAP_WRITE) && !(flags & (IOMAP_FAULT | IOMAP_ZERO));
	u64 began = written_by_write ? ktime_get_ns() : 0;

	if (!(flags & (IOMAP_WRITE | IOMAP_ZERO))) {
		if (iomap->private)
			cladefs_dax_put_range(dax, iomap->private);
		return 0;
	}
	if (written_by_write && written > 0 && pos + written > i_size_read(inode)) {
		i_size_write(inode, pos + written);
		__mark_inode_dirty(inode, I_DIRTY_SYNC | I_DIRTY_DATASYNC);
	}
	if (iomap->private) {
		cladefs_nofs_up_read(&file->sem);
		cladefs_dax_put_range(dax, iomap->private);
	}
	/* A fault's PTE in, its page's DAX entry held still (pause points). */
	if (flags & IOMAP_FAULT)
		cladefs_dax_pause_file(CLADEFS_DAX_STEP_FAULTED, inode->i_ino);
	if (written_by_write)
		atomic64_add(ktime_get_ns() - began, &dax->write_end_ns);
	return 0;
}

static const struct iomap_ops cladefs_dax_iomap_ops = {
	.iomap_begin = cladefs_dax_iomap_begin,
	.iomap_end = cladefs_dax_iomap_end,
};

/*
 * cladefs: dax_iomap_rw, dax_iomap_fault and dax_zero_range in a NOFS scope, which a file's
 * semaphore is held in (cladefs_nofs_held): a write's iomap holds it from its begin to its end,
 * across the kernel's copy or insert between.
 */
static ssize_t cladefs_dax_iomap_rw(struct kiocb *iocb, struct iov_iter *iter)
{
	cladefs_nofs_scope();
	return dax_iomap_rw(iocb, iter, &cladefs_dax_iomap_ops);
}

static vm_fault_t cladefs_dax_iomap_fault(struct vm_fault *vmf, unsigned int order,
					unsigned long *pfn, int *error)
{
	cladefs_nofs_scope();
	return dax_iomap_fault(vmf, order, pfn, error, &cladefs_dax_iomap_ops);
}

static int cladefs_dax_iomap_zero(struct inode *inode, loff_t pos, loff_t len, bool *zeroed)
{
	cladefs_nofs_scope();
	return dax_zero_range(inode, pos, len, zeroed, &cladefs_dax_iomap_ops);
}

static ssize_t cladefs_dax_read(struct kiocb *iocb, struct iov_iter *to)
{
	return cladefs_dax_iomap_rw(iocb, to);
}

/*
 * cladefs: a read, whose copy takes no page fault: a read into a mapping of the file it reads holds
 * the range through its copy, which a fault on it may wait for, or the file's semaphore shared,
 * which a writeback waits behind. It faults the destination in with no iomap held, as a write
 * does its source.
 */
static ssize_t cladefs_dax_read_faulting_between(struct kiocb *iocb, struct iov_iter *to)
{
	struct inode *inode = file_inode(iocb->ki_filp);
	ssize_t ret, done = 0;
	unsigned int empty = 0;

	for (;;) {
		pagefault_disable();
		ret = cladefs_dax_read(iocb, to);
		pagefault_enable();
		if (ret > 0) {
			done += ret;
			empty = 0;
		} else if (ret != -EFAULT && ret != 0) {
			break;
		}
		if (!iov_iter_count(to) || iocb->ki_pos >= i_size_read(inode))
			break;
		/* Nothing copied, memory there, and again: as a write's source, it won't get on. */
		if (ret <= 0 && ++empty > CLADEFS_DAX_MAX_DROPS) {
			ret = -EFAULT;
			break;
		}
		if (fault_in_iov_iter_writeable(to, iov_iter_count(to)) == iov_iter_count(to)) {
			ret = -EFAULT;
			break;
		}
	}
	return done ?: ret;
}

/* A read(2), holding the inode's lock shared. */
static ssize_t cladefs_dax_read_iter(struct kiocb *iocb, struct iov_iter *to)
{
	struct inode *inode = file_inode(iocb->ki_filp);
	ssize_t ret;

	if (!iov_iter_count(to))
		return 0;
	if (iocb->ki_flags & IOCB_NOWAIT) {
		if (!inode_trylock_shared(inode))
			return -EAGAIN;
	} else {
		inode_lock_shared(inode);
	}
	ret = cladefs_dax_read_faulting_between(iocb, to);
	inode_unlock_shared(inode);
	file_accessed(iocb->ki_filp);
	return ret;
}

/* cladefs: bytes of a write, in kernel memory, for as long as it is writing them. */
struct cladefs_dax_source {
	struct kvec kvec;
	struct iov_iter iter;
};

/*
 * cladefs: takes the next of a write's bytes at `pos`, as many as reach the chunk's end, from `from`
 * into `source`, which the write then copies from, a copy having stopped short
 * (cladefs_dax_write_iter). With page faults enabled, holding no iomap, so that a fault on a mapping
 * of the file itself gets what it needs. -EFAULT if the user's memory holds none of them; if it
 * holds fewer, those are what the write writes.
 */
static int cladefs_dax_source_take(struct cladefs_dax_source *source, struct iov_iter *from, loff_t pos)
{
	size_t len = min_t(loff_t, iov_iter_count(from), round_up(pos + 1, CLADEFS_DAX_SIZE) - pos);

	/* The first's as large as any next: what's left of a write only goes down. */
	if (!source->kvec.iov_base)
		source->kvec.iov_base = cladefs_nofs(kvmalloc(min_t(size_t, iov_iter_count(from),
								 CLADEFS_DAX_SIZE), GFP_KERNEL));
	if (!source->kvec.iov_base)
		return -ENOMEM;
	source->kvec.iov_len = copy_from_iter(source->kvec.iov_base, len, from);
	if (!source->kvec.iov_len)
		return -EFAULT;
	iov_iter_kvec(&source->iter, ITER_SOURCE, &source->kvec, 1, source->kvec.iov_len);
	return 0;
}

/* cladefs: adds to `into` the time since `*at`, and moves `*at` on. */
static void cladefs_dax_lap(atomic64_t *into, u64 *at)
{
	u64 now = ktime_get_ns();

	atomic64_add(now - *at, into);
	*at = now;
}

/*
 * cladefs: whether a write(2) at `pos` follows on from `file`'s last, beginning where it ended or
 * within its last CLADEFS_DAX_FOLLOWS bytes, as a log's appends do (cladefs_dax_write_follows). Under the
 * inode's write lock as it is decided; read without it ahead of the lock, a guess
 * (cladefs_dax_private_before).
 */
static bool cladefs_dax_follows_on(struct cladefs_dax_file *file, loff_t pos)
{
	loff_t write_pos = READ_ONCE(file->write_pos), write_end = READ_ONCE(file->write_end);

	return write_end > 0 && pos >= max(write_pos, write_end - (loff_t)CLADEFS_DAX_FOLLOWS) &&
	       pos <= write_end;
}

/*
 * cladefs: the pages a PRIVATE makes private ahead of a write(2) of the bytes from `pos` to `end`
 * (cladefs_dax_write_ahead), as readahead's window grows: a write that follows on from the file's
 * last, beginning where it ended or within its last CLADEFS_DAX_FOLLOWS bytes, as a log's appends do,
 * rewriting the block they share, takes twice as many as the last, CLADEFS_DAX_AHEAD_FIRST at first,
 * CLADEFS_DAX_AHEAD_MOST at most; any other none, a rewrite of what the last wrote further back
 * among them. Under the inode's write lock (TASK-164.10).
 */
static void cladefs_dax_write_follows(struct cladefs_dax_file *file, loff_t pos, loff_t end)
{
	unsigned int ahead = 0;

	if (cladefs_dax_follows_on(file, pos))
		ahead = clamp(2 * file->ahead, CLADEFS_DAX_AHEAD_FIRST, CLADEFS_DAX_AHEAD_MOST);
	WRITE_ONCE(file->ahead, ahead);
	WRITE_ONCE(file->write_pos, pos);
	WRITE_ONCE(file->write_end, end);
}

/*
 * cladefs: makes the pages of `inode`'s chunk `chunk` from `pos` to `end` that a write(2) is about
 * to store to private, clean, as a PRIVATE ahead of a write does (cladefs_dax_dirty): those of a
 * range of the file's own already, mapped and written, if there is room for them below where the
 * budget's worker begins. Holding the inode's invalidate lock and the file's semaphore shared, as
 * a fault's iomap does, not the inode's lock (cladefs_dax_private_before). 0, or the PRIVATE's
 * failure, as cladefs_dax_dirty fails it: -ENOMEM if the guest had no memory for it, else -EIO.
 */
static int cladefs_dax_private_chunk_before(struct inode *inode, pgoff_t chunk, loff_t pos,
					 loff_t end)
{
	struct cladefs_dax_file *file = cladefs_dax_file(inode);
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	struct cladefs_dax_range *range;
	unsigned long first, last;
	long newly, made;
	loff_t start;
	int err = 0;
	cladefs_nofs_scope();

	filemap_invalidate_lock_shared(inode->i_mapping);
	cladefs_nofs_down_read(&file->sem);
	range = xa_load(&file->ranges, chunk);
	/* A slice's chunk is mapped again as a range of its own by the write's iomap. */
	if (!range || range->region)
		goto out;
	start = cladefs_dax_chunk_start(range);
	first = (pos - start) >> PAGE_SHIFT;
	last = min_t(unsigned long, range->pages, DIV_ROUND_UP(end - start, PAGE_SIZE));
	if (first >= last)
		goto out;
	last = cladefs_dax_pages_aheadable_run(range->state, first, last);
	if (first >= last)
		goto out;
	newly = cladefs_dax_pages_would_dirty(range->state, first, last - first).private;
	if (!newly || atomic_long_read(&dax->private) + newly >
			      dax->private_max - dax->private_max / 8 ||
	    !cladefs_dax_take_private(dax, newly))
		goto out;
	/* The pages private already, those others' iomaps made so meanwhile too, stay so. */
	err = cladefs_device_private(dax->dev, cladefs_dax_page_offset(range, first),
				  (u64)(last - first) << PAGE_SHIFT);
	atomic64_inc(&dax->privates);
	if (err) {
		err = cladefs_dax_private_failed(dax, cladefs_dax_page_offset(range, first), last - first,
					      newly, err);
		goto out;
	}
	made = cladefs_dax_pages_private_ahead(range->state, first, last - first);
	atomic64_add(made, &dax->private_before);
	/* Others' iomaps, holding the semaphore shared too, may have counted some meanwhile. */
	if (newly > made)
		atomic_long_sub(newly - made, &dax->private);
	atomic_long_add(made, &file->private_pages);
	WRITE_ONCE(file->dirtied, jiffies);
out:
	cladefs_nofs_up_read(&file->sem);
	filemap_invalidate_unlock_shared(inode->i_mapping);
	return err;
}

/*
 * cladefs: makes the pages a write(2) of `count` bytes at `pos` stores to private before it takes the
 * inode's lock (cladefs_dax_private_chunk_before), as a fault makes its page private without it: a
 * write's PRIVATE, a round trip to the host of half a millisecond nested, held the lock, and the
 * file's other writes waited for it as long as the PRIVATEs took (TASK-164.11; a forked pgbench
 * on GCE, r802: 160 s of waits for 163 s of PRIVATEs). Its iomap, under the lock, finds them
 * private and asks for nothing. What it leaves, as a chunk not mapped yet, the file's end moving,
 * no room below the budget's worker, or rebinding meanwhile, the iomap makes private as before; a
 * PRIVATE failed fails the write, as the iomap's would (cladefs_dax_dirty). Not a write that follows
 * on from the file's last, a log's, whose iomap's PRIVATE takes the pages its next writes reach
 * too (cladefs_dax_write_ahead), which one made here would leave it none to ask.
 */
static int cladefs_dax_private_before(struct inode *inode, loff_t pos, size_t count)
{
	loff_t end = min_t(loff_t, pos + count, i_size_read(inode)), next;
	int err = 0;

	if (cladefs_dax_follows_on(cladefs_dax_file(inode), pos))
		return 0;
	for (; !err && pos < end; pos = next) {
		pgoff_t chunk = pos >> CLADEFS_DAX_SHIFT;

		next = min_t(loff_t, end, (loff_t)(chunk + 1) << CLADEFS_DAX_SHIFT);
		err = cladefs_dax_private_chunk_before(inode, chunk, pos, next);
	}
	return err;
}

/*
 * Writes through the window, copying with page faults disabled: the copy holds the file's
 * semaphore shared (cladefs_dax_iomap_write), and a fault on the user's memory, a mapping of the file
 * itself, would take it again, behind a writeback waiting for it. The memory is faulted in first,
 * and again whenever a copy stops short. cladefs: and bytes are taken in hand, the rest of the
 * chunk's, and copied from there, a copy that can't stop short, once a copy stopped short with
 * none copied (-EFAULT): what faulted in may be gone again. Then the write goes back to the user's
 * memory.
 */
static ssize_t cladefs_dax_write_iter(struct kiocb *iocb, struct iov_iter *from)
{
	struct inode *inode = file_inode(iocb->ki_filp);
	struct cladefs_dax_file *file = cladefs_dax_file(inode);
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	struct cladefs_dax_source hand = {};
	struct iov_iter *source = from;
	loff_t old_size, end;
	u64 began, lap;
	ssize_t ret, done = 0;
	unsigned int drops = 0, throttles = 0;
	bool made_room = false;
	int err;

	if (iocb->ki_flags & IOCB_NOWAIT)
		return -EAGAIN;
	began = lap = ktime_get_ns();
	/* An append's place is the file's end, which only the lock holds still. */
	if (!(iocb->ki_flags & IOCB_APPEND)) {
		err = cladefs_dax_private_before(inode, iocb->ki_pos, iov_iter_count(from));
		if (err)
			return err;
	}
	cladefs_dax_lap(&dax->write_private_ns, &lap);
	inode_lock(inode);
	cladefs_dax_lap(&dax->write_lock_ns, &lap);
	old_size = i_size_read(inode);
	ret = generic_write_checks(iocb, from);
	if (ret <= 0)
		goto out;
	ret = file_remove_privs(iocb->ki_filp);
	if (ret)
		goto out;
	ret = file_update_time(iocb->ki_filp);
	if (ret)
		goto out;
	end = iocb->ki_pos + iov_iter_count(from);
	cladefs_dax_write_follows(file, iocb->ki_pos, end);
	/* The ranges the file's end moves into, mapped as large as they need. */
	if (end > old_size) {
		WRITE_ONCE(file->growing, end);
		filemap_invalidate_lock(inode->i_mapping);
		ret = cladefs_dax_fit(inode, end);
		if (!ret && iocb->ki_pos > old_size)
			ret = cladefs_dax_zero_tail(inode, old_size, iocb->ki_pos);
		filemap_invalidate_unlock(inode->i_mapping);
		if (ret)
			goto out;
	}
	cladefs_dax_lap(&dax->write_setup_ns, &lap);
	while (iov_iter_count(source)) {
		bool unreadable = source == from &&
				  fault_in_iov_iter_readable(from, iov_iter_count(from)) ==
				  iov_iter_count(from);

		cladefs_dax_lap(&dax->write_fault_in_ns, &lap);
		if (unreadable) {
			ret = -EFAULT;
			break;
		}
		pagefault_disable();
		ret = cladefs_dax_iomap_rw(iocb, source);
		pagefault_enable();
		cladefs_dax_lap(&dax->write_rw_ns, &lap);
		if (ret > 0) {
			done += ret;
			drops = 0;
		} else if (ret == -ENOBUFS) {
			/*
			 * cladefs: no room among the private pages: waited for, holding nothing of the
			 * file's but the inode's lock (doc-13's budget).
			 */
			ret = cladefs_dax_throttle(dax, &throttles);
			if (ret)
				break;
		} else if (ret == -ENOSPC && !made_room) {
			/* cladefs: room made once, as the page cache's write_begin does. */
			cladefs_vfs_make_room(inode->i_sb);
			made_room = true;
		} else if (source == from && ret == -EFAULT) {
			ret = cladefs_dax_source_take(&hand, from, iocb->ki_pos);
			if (ret)
				break;
			source = &hand.iter;
		} else if (ret == -EXDEV) {
			/*
			 * cladefs: a write that can't get on fails, and says so: it spun unkillable, with
			 * nothing in the log (TASK-140).
			 */
			if (fatal_signal_pending(current)) {
				ret = -EINTR;
				break;
			}
			if (++drops > CLADEFS_DAX_MAX_DROPS) {
				pr_err("cladefs: ino %lu: a write at %lld found no range to take it\n",
				       inode->i_ino, iocb->ki_pos);
				ret = -EIO;
				break;
			}
			if (drops > 1)
				WRITE_ONCE(file->no_slices, true);
			/* A slice, or a range too small: dropped, the chunk is mapped again. */
			ret = cladefs_dax_drop_slice(inode, iocb->ki_pos, true);
			cladefs_dax_lap(&dax->write_retry_ns, &lap);
			if (ret)
				break;
		} else {
			break;
		}
		if (source != from && !iov_iter_count(source))
			source = from;
	}
	/* What it took and didn't write, it didn't write. */
	if (source != from)
		iov_iter_revert(from, iov_iter_count(source));
	kvfree(hand.kvec.iov_base);
	/*
	 * A write that stopped short leaves the file no longer than it wrote: one that wrote nothing
	 * leaves it as it was, wherever it began past the end.
	 */
	if (iocb->ki_pos < end && end > old_size) {
		err = cladefs_data_resize(inode, done ? max(old_size, iocb->ki_pos) : old_size, NULL, NULL);
		if (err && !done)
			ret = err;
	}
	if (done)
		ret = done;
	cladefs_dax_lap(&dax->write_tail_ns, &lap);
out:
	WRITE_ONCE(file->no_slices, false);
	WRITE_ONCE(file->growing, 0);
	inode_unlock(inode);
	began = ktime_get_ns() - began;
	atomic64_inc(&dax->writes);
	atomic64_add(began, &dax->write_ns);
	cladefs_hist_add(&dax->write_hist, began);
	if (ret > 0)
		ret = generic_write_sync(iocb, ret);
	return ret;
}

/* A fault of `order`, PAGE's or PMD's, writing if `write`. */
static vm_fault_t cladefs_dax_do_fault(struct vm_fault *vmf, unsigned int order, bool write)
{
	struct inode *inode = file_inode(vmf->vma->vm_file);
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	struct super_block *sb = inode->i_sb;
	u64 began = ktime_get_ns();
	unsigned int waits = 0, drops = 0, throttles = 0;
	bool made_room = false;
	vm_fault_t ret;
	unsigned long pfn;
	int error;

	trace_cladefs_dax_fault_start(inode->i_ino, vmf->pgoff, order, write);
	if (write) {
		sb_start_pagefault(sb);
		file_update_time(vmf->vma->vm_file);
	}
	for (;;) {
		long seen = atomic_long_read(&dax->released);

		/*
		 * cladefs: each try after the first yields first: retries that find what they wait for
		 * busy as soon as they look again kept the task off both of a 2-vCPU guest's CPUs,
		 * without preemption, for minutes (TASK-113).
		 */
		cond_resched();
		error = 0;
		filemap_invalidate_lock_shared(inode->i_mapping);
		ret = cladefs_dax_iomap_fault(vmf, order, &pfn, &error);
		filemap_invalidate_unlock_shared(inode->i_mapping);
		if (!(ret & VM_FAULT_ERROR))
			break;
		/*
		 * cladefs: no room among the private pages: waited for, holding nothing of the file's,
		 * as balance_dirty_pages after a fault (doc-13's budget).
		 */
		if (error == -ENOBUFS) {
			error = cladefs_dax_throttle(dax, &throttles);
			if (error) {
				ret = VM_FAULT_SIGBUS;
				break;
			}
			continue;
		}
		/*
		 * cladefs: no room in Fxfs for the page's writeback: once, room made by writing back
		 * what is dirty, as the page cache's page_mkwrite does (cladefs_vfs_make_room).
		 */
		if (error == -ENOSPC && !made_room) {
			cladefs_vfs_make_room(sb);
			made_room = true;
			continue;
		}
		if (error != -EAGAIN && error != -EXDEV)
			break;
		/* cladefs: a fault killed meanwhile tries no more (TASK-79.49). */
		if (fatal_signal_pending(current)) {
			ret = VM_FAULT_SIGBUS;
			break;
		}
		if (error == -EXDEV) {
			/*
			 * A slice can't take the store: it maps the chunk no more. cladefs: a fault that
			 * can't get on fails, and says so, as a write does (TASK-140).
			 */
			if (++drops > CLADEFS_DAX_MAX_DROPS)
				break;
			ret = cladefs_dax_fault_drop_slice(vmf, ret);
			if (ret)
				break;
			continue;
		}
		/*
		 * No range was free, nor could the fault reclaim one, holding the lock it holds.
		 * cladefs: nor, for so long, could anything: it fails rather than wait for ever.
		 */
		if (++waits > CLADEFS_DAX_NO_ROOM_WAITS)
			break;
		cladefs_dax_make_room(dax, seen);
	}
	/* cladefs: a SIGBUS says nothing of why: the error here does (TASK-121). */
	if (ret & VM_FAULT_SIGBUS)
		pr_err_ratelimited("cladefs: a %s fault of order %u at page %lu of ino %lu failed: %d\n",
				   write ? "write" : "read", order, vmf->pgoff, inode->i_ino, error);
	if (write)
		sb_end_pagefault(sb);
	cladefs_hist_add(&dax->fault_hist, ktime_get_ns() - began);
	trace_cladefs_dax_fault_end(inode->i_ino, vmf->pgoff, ret);
	return ret;
}

/* A write to a private mapping writes its own copy of the page, which the fault reads: as ext4's. */
static vm_fault_t cladefs_dax_fault(struct vm_fault *vmf)
{
	return cladefs_dax_do_fault(vmf, 0, (vmf->flags & FAULT_FLAG_WRITE) &&
						 (vmf->vma->vm_flags & VM_SHARED));
}

/*
 * cladefs: a read fault of a PMD-aligned 2 MiB of a mapping maps them with one PMD, read-only, where
 * the window holds them in a row, as it does a chunk of a large file mapped whole (the iomap says
 * so); otherwise the kernel faults them a page at a time (VM_FAULT_FALLBACK, TASK-26.18.8). A store
 * falls back to pages, so that dirty pages are 4 KiB, and writeback write-protects them page by
 * page (cladefs_dax_protect).
 */
static vm_fault_t cladefs_dax_huge_fault(struct vm_fault *vmf, unsigned int order)
{
	if (order != PMD_ORDER || (vmf->flags & FAULT_FLAG_WRITE))
		return VM_FAULT_FALLBACK;
	return cladefs_dax_do_fault(vmf, order, false);
}

static vm_fault_t cladefs_dax_mkwrite(struct vm_fault *vmf)
{
	return cladefs_dax_do_fault(vmf, 0, true);
}

static const struct vm_operations_struct cladefs_dax_vm_ops = {
	.fault = cladefs_dax_fault,
	.huge_fault = cladefs_dax_huge_fault,
	.page_mkwrite = cladefs_dax_mkwrite,
	.pfn_mkwrite = cladefs_dax_mkwrite,
};

static int cladefs_dax_mmap(struct file *file, struct vm_area_struct *vma)
{
	file_accessed(file);
	vma->vm_ops = &cladefs_dax_vm_ops;
	/*
	 * cladefs: read faults may map 2 MiB at once (cladefs_dax_huge_fault), as ext4's DAX does. And as
	 * ext4's, not VM_MIXEDMAP, which the folios dax.c maps need not be: a fork copies the PTEs of
	 * a mapping that is, a large folio's as one, all writable if any is (copy_present_ptes,
	 * FPB_MERGE_WRITE), which made writable, unfaulted, pages no PRIVATE had made the VM's: those
	 * of a chunk read in whole, a store having split its PMD. The child's store to one the host
	 * refused, again and again (GCE r861, TASK-149.16). A child's PTEs are its own faults'.
	 */
	vm_flags_set(vma, VM_HUGEPAGE);
	return 0;
}

/*
 * cladefs: the file's dirty pages written back (cladefs_dax_write_back), whatever `wbc` asks: the
 * flusher's, every 30 s or so, and fsync's and sync's (doc-13); then the kernel's dirty tags
 * cleaned, which say nothing to dax.c, so that the kernel takes the inode for clean
 * (cladefs_dax_clean_entries).
 */
static int cladefs_dax_writepages(struct address_space *mapping, struct writeback_control *wbc)
{
	struct inode *inode = mapping->host;
	int err, cleaned;

	err = cladefs_dax_write_back(inode);
	cleaned = cladefs_dax_clean_entries(inode, 0, LLONG_MAX);
	return err ?: cleaned;
}

static const struct address_space_operations cladefs_dax_aops = {
	.writepages = cladefs_dax_writepages,
	.dirty_folio = noop_dirty_folio,
	/* O_DIRECT opens: reads and writes through the window are direct already, as ext4's. */
	.direct_IO = noop_direct_IO,
};

static const struct file_operations cladefs_dax_file_operations = {
	.open = generic_file_open,
	.llseek = cladefs_vfs_llseek,
	.read_iter = cladefs_dax_read_iter,
	.write_iter = cladefs_dax_write_iter,
	.mmap = cladefs_dax_mmap,
	/* cladefs: mappings 2 MiB-aligned, as the window's chunks are, for PMD faults. */
	.get_unmapped_area = thp_get_unmapped_area,
	.splice_read = copy_splice_read,
	.splice_write = iter_file_splice_write,
	.fsync = cladefs_vfs_fsync,
	.fallocate = cladefs_data_fallocate,
	.remap_file_range = cladefs_data_remap_file_range,
};

/*
 * cladefs: once a seal ends: page-cache inodes evicted before it may be read in to DAX, which the
 * worker finds (cladefs_dax_prune).
 */
static void cladefs_dax_sealed(void *arg)
{
	struct cladefs_dax *dax = arg;

	/* cladefs: between the seal's end and what follows it (TASK-119.3). */
	cladefs_dax_delay();
	if (atomic_long_read(&dax->unsealed))
		cladefs_dax_kick(dax, false);
}

/*
 * Destroys `pool`, whatever of it is taken: gen_pool_destroy stops the kernel if anything is, and
 * the window's pages an UNMAP failed for are kept taken for good (cladefs_dax_give_unmapped,
 * TASK-150.12).
 */
static void cladefs_dax_destroy_pool(struct gen_pool *pool)
{
	struct gen_pool_chunk *chunk;

	list_for_each_entry(chunk, &pool->chunks, next_chunk)
		bitmap_zero(chunk->bits,
			    (chunk->end_addr - chunk->start_addr + 1) >> pool->min_alloc_order);
	gen_pool_destroy(pool);
}

/* Destroys the window's pools: the ranges', and the regions', if apart. */
static void cladefs_dax_destroy_pools(struct cladefs_dax *dax)
{
	if (dax->region_pool && dax->region_pool != dax->pool)
		cladefs_dax_destroy_pool(dax->region_pool);
	if (dax->pool)
		cladefs_dax_destroy_pool(dax->pool);
}

struct cladefs_dax *cladefs_dax_create(struct cladefs_filesystem *filesystem)
{
	struct cladefs_device *dev = cladefs_dax_device(filesystem);
	struct dax_device *dax_dev;
	struct cladefs_dax *dax;
	u64 window_len;

	dax_dev = cladefs_device_dax(dev, &window_len);
	if (!dax_dev || window_len < CLADEFS_DAX_SIZE)
		return NULL;
	/* cladefs: a few iomaps' worth of private pages at least (doc-13). */
	if (cladefs_device_private_max(dev) < 8) {
		pr_warn("cladefs: the DAX window holds %llu private pages: no DAX\n",
			cladefs_device_private_max(dev));
		return NULL;
	}
	dax = kzalloc(sizeof(*dax), GFP_KERNEL);
	if (!dax)
		return ERR_PTR(-ENOMEM);
	dax->pool = gen_pool_create(PAGE_SHIFT, -1);
	/*
	 * cladefs: half a window of four chunks or more, in chunks, for regions (TASK-91.10): a quarter
	 * filled as pgbench -i -s 50 loaded its tables, whose writes then mapped their pieces in
	 * ranges' space, the load half as slow again.
	 */
	dax->window_len = window_len;
	dax->regions_at = window_len < 4 * CLADEFS_DAX_SIZE ? window_len :
			  window_len - rounddown(window_len / 2, CLADEFS_DAX_SIZE);
	dax->region_pool = dax->regions_at < window_len ? gen_pool_create(PAGE_SHIFT, -1) : dax->pool;
	if (!dax->pool || !dax->region_pool ||
	    gen_pool_add(dax->pool, CLADEFS_DAX_POOL_BASE, dax->regions_at, -1) ||
	    (dax->region_pool != dax->pool &&
	     gen_pool_add(dax->region_pool, CLADEFS_DAX_POOL_BASE + dax->regions_at,
			  window_len - dax->regions_at, -1))) {
		cladefs_dax_destroy_pools(dax);
		kfree(dax);
		return ERR_PTR(-ENOMEM);
	}
	dax->filesystem = filesystem;
	dax->dev = dev;
	dax->dax_dev = dax_dev;
	INIT_LIST_HEAD(&dax->volumes);
	mutex_init(&dax->files_lock);
	spin_lock_init(&dax->lock);
	INIT_LIST_HEAD(&dax->used);
	init_waitqueue_head(&dax->wait);
	atomic_long_set(&dax->released, 0);
	hash_init(dax->regions);
	spin_lock_init(&dax->regions_lock);
	dax->wq = alloc_workqueue("cladefs_dax", WQ_UNBOUND | WQ_MEM_RECLAIM, 0);
	if (!dax->wq) {
		cladefs_dax_destroy_pools(dax);
		kfree(dax);
		return ERR_PTR(-ENOMEM);
	}
	/* cladefs: the budget's worker, and the slice of the budget an iomap takes (doc-13). */
	INIT_WORK(&dax->work, cladefs_dax_work);
	mutex_init(&dax->kick_lock);
	init_waitqueue_head(&dax->budget_wait);
	dax->private_max = min_t(u64, cladefs_device_private_max(dev), LONG_MAX);
	dax->iomap_pages = clamp_t(unsigned long, dax->private_max / 8, 1, CLADEFS_DAX_PRIVATE_PAGES);
	cladefs_device_set_sealed(dev, cladefs_dax_sealed, dax);
	cladefs_dax_publish(dax);
	return dax;
}

struct cladefs_dax_volume *cladefs_dax_add_volume(struct cladefs_dax *dax, struct cladefs *fs)
{
	struct cladefs_dax_volume *volume = kzalloc(sizeof(*volume), GFP_KERNEL);

	if (!volume)
		return ERR_PTR(-ENOMEM);
	volume->fs = fs;
	xa_init(&volume->files);
	xa_init(&volume->unsealed);
	mutex_lock(&dax->files_lock);
	list_add(&volume->list, &dax->volumes);
	mutex_unlock(&dax->files_lock);
	return volume;
}

void cladefs_dax_remove_volume(struct cladefs_dax *dax, struct cladefs_dax_volume *volume)
{
	struct cladefs_dax_range *range;
	struct cladefs_dax_file *file;
	unsigned long ino, chunk;
	void *entry;
	cladefs_nofs_scope();

	/* cladefs: the budget's worker, which uses the volume's files, stopped by the caller. */
	WARN_ON_ONCE(!READ_ONCE(dax->stops));
	/* Reclaim may hold one of its files still: it finds it has no volume. */
	mutex_lock(&dax->files_lock);
	list_del(&volume->list);
	xa_for_each(&volume->files, ino, file) {
		WARN_ON_ONCE(file->inode);
		file->volume = NULL;
	}
	mutex_unlock(&dax->files_lock);

	/* Its inodes evicted, nothing uses its ranges. */
	xa_for_each(&volume->files, ino, file) {
		xa_erase(&volume->files, ino);
		cladefs_nofs_down_write(&file->sem);
		xa_for_each(&file->ranges, chunk, range) {
			if (WARN_ON_ONCE(atomic_read(&range->users)))
				continue;
			cladefs_dax_unmap_range(dax, range);
		}
		cladefs_nofs_up_write(&file->sem);
		cladefs_dax_put_file(dax, file);
	}
	xa_destroy(&volume->files);
	xa_for_each(&volume->unsealed, ino, entry)
		atomic_long_dec(&dax->unsealed);
	xa_destroy(&volume->unsealed);
	kfree(volume);
}

/*
 * cladefs: before a sync seals what is written, every file's dirty pages written back, which a
 * write(2)'s leave the mapping untagged for, so that sync's writepages miss them (doc-13); and
 * those of a writeback under way committed, which cladefs_dax_write_back waits for: a file whose
 * pages the budget's writeback had swapped out had none dirty, and was sealed without them. Each
 * holding dirty or private pages, as a writeback's are until a rebind after its commit.
 */
int cladefs_dax_settle(struct cladefs_dax *dax)
{
	struct cladefs_dax_file **files;
	ssize_t nr, i;
	int err = 0;

	nr = cladefs_dax_busy_files(dax, &files);
	if (nr < 0)
		return nr;
	for (i = 0; i < nr; i++) {
		struct inode *inode = cladefs_dax_inode_of(dax, files[i]);

		if (inode) {
			int wrote = cladefs_dax_write_back(inode);

			err = err ?: wrote;
			iput(inode);
		}
		cladefs_dax_put_file(dax, files[i]);
	}
	kvfree(files);
	return err;
}

/*
 * cladefs: stops the budget's worker, and waits for it: before a volume's inodes are evicted, or
 * the filesystem freezes. A write it would have made room for waits meanwhile. Stops are counted,
 * each held until the cladefs_dax_start that balances it, whatever other stops and starts come
 * meanwhile: one flag, a volume's leave cleared the stop of another volume's eviction under way,
 * and the worker's relieve took that volume's inodes (board 12453).
 */
void cladefs_dax_stop(struct cladefs_dax *dax)
{
	/* cladefs: under the kicks' lock, so that none queues a round once it is set (board 10373). */
	mutex_lock(&dax->kick_lock);
	WRITE_ONCE(dax->stops, dax->stops + 1);
	mutex_unlock(&dax->kick_lock);
	cancel_work_sync(&dax->work);
}

/*
 * cladefs: balances one cladefs_dax_stop, and has the worker run once no other stop holds it: a
 * frozen filesystem's does, until its thaw.
 */
void cladefs_dax_start(struct cladefs_dax *dax)
{
	bool started;

	mutex_lock(&dax->kick_lock);
	if (!WARN_ON_ONCE(!dax->stops))
		WRITE_ONCE(dax->stops, dax->stops - 1);
	started = !dax->stops;
	mutex_unlock(&dax->kick_lock);
	if (started)
		cladefs_dax_kick(dax, false);
}

/*
 * cladefs: the filesystem frozen, durably, its writers stopped and their pages written back as the
 * VFS synced it: every private page is rebound, mapping its extent's page, sealed now, so that the
 * snapshot's window holds extents and zeros alone, as the device has it (doc-13). Those it can't,
 * as a writeback that found no memory leaves dirty (TASK-207) or a rebind refused, warned of: the
 * host refuses the snapshot's point while the window holds any (EBUSY, the extent daemon saying
 * how many: ExtentStore::point, TASK-203), and the VM goes on as it was.
 */
void cladefs_dax_frozen(struct cladefs_dax *dax)
{
	cladefs_dax_relieve(dax, 0, false);
	if (atomic_long_read(&dax->private))
		pr_warn("cladefs: frozen with %ld private pages, %ld dirty: no snapshot\n",
			atomic_long_read(&dax->private), atomic_long_read(&dax->dirty));
}

/*
 * Stops the budget's worker, once nothing writes any more, before the filesystem freezes, while
 * its device takes requests: until cladefs_dax_thaw.
 */
void cladefs_dax_quiesce(struct cladefs_dax *dax)
{
	cladefs_dax_stop(dax);
}

/* Starts the budget's worker again, as the filesystem thaws (cladefs_dax_quiesce). */
void cladefs_dax_thaw(struct cladefs_dax *dax)
{
	cladefs_dax_start(dax);
}

/*
 * Frees `dax` once the filesystem is unmounted, its inodes evicted: the window's ranges are
 * unmapped as the device goes.
 */
void cladefs_dax_destroy(struct cladefs_dax *dax)
{
	struct cladefs_dax_volume *volume, *next_volume;
	struct cladefs_dax_range *range, *next;
	struct cladefs_dax_region *region;
	struct cladefs_dax_file *file;
	struct hlist_node *tmp;
	unsigned long ino;
	int bucket;

	/* cladefs: first, so that no knob kicks the worker once it stops (board 8555). */
	cladefs_dax_unpublish(dax);
	/* As cladefs_dax_quiesce, where mounting failed. */
	cladefs_dax_stop(dax);
	list_for_each_entry_safe(range, next, &dax->used, list) {
		xa_erase(&range->file->ranges, range->chunk);
		list_del(&range->list);
		/* Its regions go below, and its space with the pools. */
		cladefs_dax_free_range_memory(range);
	}
	hash_for_each_safe(dax->regions, bucket, tmp, region, node) {
		hash_del(&region->node);
		kfree(region);
	}
	list_for_each_entry_safe(volume, next_volume, &dax->volumes, list) {
		xa_for_each(&volume->files, ino, file) {
			xa_erase(&volume->files, ino);
			cladefs_dax_put_file(dax, file);
		}
		xa_destroy(&volume->files);
		xa_destroy(&volume->unsealed);
		kfree(volume);
	}
	destroy_workqueue(dax->wq);
	cladefs_dax_destroy_pools(dax);
	kfree(dax);
}

void cladefs_dax_inode_init(struct cladefs_dax_inode *di)
{
	di->file = NULL;
}

/*
 * The file `ino`, whose ranges outlive its inodes, made if it has none, with `inode` read in for
 * it; NULL without the memory for it.
 */
static struct cladefs_dax_file *cladefs_dax_attach(struct cladefs_dax *dax, struct inode *inode)
{
	struct cladefs_dax_volume *volume = cladefs_dax_volume_of(inode);
	struct cladefs_dax_file *file;

	mutex_lock(&dax->files_lock);
	file = xa_load(&volume->files, inode->i_ino);
	if (!file) {
		file = kzalloc(sizeof(*file), GFP_NOFS);
		if (!file)
			goto out;
		file->volume = volume;
		file->ino = inode->i_ino;
		refcount_set(&file->refs, 1);
		init_rwsem(&file->sem);
		mutex_init(&file->wb_lock);
		xa_init(&file->ranges);
		if (xa_err(xa_store(&volume->files, inode->i_ino, file, GFP_NOFS))) {
			kfree(file);
			file = NULL;
			goto out;
		}
	}
	WARN_ON_ONCE(file->inode);
	file->inode = inode;
out:
	mutex_unlock(&dax->files_lock);
	return file;
}

/*
 * cladefs: whether the page-cache inode `ino`, of `volume`, has writes no barrier has sealed yet
 * (`unsealed`): read in now, the window would find their extents open, which it maps none of
 * (doc-13).
 */
static bool cladefs_dax_unsealed(struct cladefs_dax *dax, struct cladefs_dax_volume *volume, u64 ino)
{
	void *entry = xa_load(&volume->unsealed, ino);

	if (!entry)
		return false;
	if (cladefs_device_seals(dax->dev) < xa_to_value(entry))
		return true;
	if (xa_cmpxchg(&volume->unsealed, ino, entry, NULL, GFP_NOFS) == entry)
		atomic_long_dec(&dax->unsealed);
	return false;
}

void cladefs_dax_written(struct inode *inode)
{
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	struct cladefs_dax_volume *volume;
	void *was;

	if (!dax || IS_DAX(inode))
		return;
	volume = cladefs_dax_volume_of(inode);
	was = xa_store(&volume->unsealed, inode->i_ino, xa_mk_value(cladefs_dax_seal_target(dax)),
		       GFP_NOFS);
	/* Without the memory, it is read in to DAX, whose mapping seals what it finds open. */
	if (!xa_is_err(was) && !was)
		atomic_long_inc(&dax->unsealed);
}

bool cladefs_dax_set_inode(struct inode *inode, bool created)
{
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	struct cladefs_dax_file *file = NULL;

	/*
	 * cladefs: an inode created in this mount is a page-cache inode while it is in memory, as one
	 * whose writes no barrier has sealed yet, read in again: S_DAX is set as an inode is read in,
	 * never on a live one, as ext4 and XFS have it (doc-13, TASK-164.3).
	 */
	if (created || cladefs_dax_unsealed(dax, cladefs_dax_volume_of(inode), inode->i_ino))
		return false;
	if (!cladefs_dax_should_fail_attach())
		file = cladefs_dax_attach(dax, inode);
	/* Without its file, it stays in the page cache. */
	if (!file)
		return false;
	cladefs_vfs_inode(inode)->dax.file = file;
	cladefs_vfs_inode(inode)->data = &cladefs_dax_data_ops;
	inode->i_flags |= S_DAX;
	inode->i_fop = &cladefs_dax_file_operations;
	inode->i_mapping->a_ops = &cladefs_dax_aops;
	return true;
}

/* cladefs: how often, and how far apart, an eviction tries again a writeback that found no memory. */
#define CLADEFS_DAX_EVICT_TRIES 100
#define CLADEFS_DAX_EVICT_WAIT_MS 10

/*
 * Detaches `inode`, which the kernel evicts, from its file, whose ranges stay mapped for the next
 * inode read in for it, its dirty pages written back first: unless no name links it, when they go.
 */
void cladefs_dax_evict(struct inode *inode)
{
	struct cladefs_dax_file *file = cladefs_dax_file(inode);
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	struct cladefs_dax_range *range;
	unsigned long chunk;
	unsigned int tries;
	bool forgotten;
	int err;
	cladefs_nofs_scope();

	if (!dax || !IS_DAX(inode) || !file)
		return;
	if (inode->i_nlink) {
		/*
		 * cladefs: what Fxfs had no memory for stays dirty (TASK-207), which no writeback
		 * finds once the inode is gone: tried again, a while, then given up on, as an I/O
		 * error's are.
		 */
		err = cladefs_dax_write_back(inode);
		for (tries = 0; err == -ENOMEM && tries < CLADEFS_DAX_EVICT_TRIES; tries++) {
			msleep(CLADEFS_DAX_EVICT_WAIT_MS);
			err = cladefs_dax_write_back(inode);
		}
		if (err == -ENOMEM)
			err = cladefs_dax_write_back_keeping(inode, false);
		if (err)
			pr_warn("cladefs: writing back ino %lu as it is evicted: %d\n", inode->i_ino, err);
		/*
		 * cladefs: every DAX entry is gone (truncate_inode_pages_final ran above), so a
		 * later read-in of this file can map clean 2 MiB PMDs again; any subsequent write
		 * will split them back to order-0 folios (board 14407).
		 */
		cladefs_nofs_down_write(&file->sem);
		xa_for_each(&file->ranges, chunk, range) {
			WRITE_ONCE(range->has_pmd, false);
			smp_store_release(&range->no_pmd, false);
		}
		cladefs_nofs_up_write(&file->sem);
	} else {
		cladefs_nofs_down_write(&file->sem);
		xa_for_each(&file->ranges, chunk, range)
			cladefs_dax_unmap_range(dax, range);
		cladefs_nofs_up_write(&file->sem);
	}
	mutex_lock(&dax->files_lock);
	file->inode = NULL;
	forgotten = cladefs_dax_forget_file(dax, file);
	mutex_unlock(&dax->files_lock);
	if (forgotten)
		cladefs_dax_put_file(dax, file);
	cladefs_vfs_inode(inode)->dax.file = NULL;
}

/* Waits for a page in use, without the invalidate_lock, as fuse_wait_dax_page. */
static void cladefs_dax_wait_page(struct inode *inode)
{
	filemap_invalidate_unlock(inode->i_mapping);
	schedule();
	filemap_invalidate_lock(inode->i_mapping);
}

/*
 * Waits for the pages of `inode` from `start` on to be unused, and removes their DAX entries, as
 * fuse_dax_break_layouts: with the invalidate_lock held for writing. From the start of its chunk:
 * a PMD's entry goes whole, and a mapping may have pages of it before `start` mapped, which
 * dax_folio_put then finds in use.
 */
static int cladefs_dax_break_layouts(struct inode *inode, loff_t start)
{
	lockdep_assert_held_write(&inode->i_mapping->invalidate_lock);

	return dax_break_layout(inode, round_down(start, CLADEFS_DAX_SIZE), LLONG_MAX,
				cladefs_dax_wait_page);
}

/*
 * Zeroes the `len` bytes of `inode` at `pos`, in one page, through the window: dropping a slice
 * that can't take them, to be mapped again as a range of its own. With the invalidate_lock held
 * for writing.
 */
static int cladefs_dax_zero_part(struct inode *inode, loff_t pos, loff_t len)
{
	unsigned int throttles = 0;
	bool zeroed;
	int err;

	for (;;) {
		err = cladefs_dax_iomap_zero(inode, pos, len, &zeroed);
		/* cladefs: no room among the private pages: the worker makes some. */
		if (err == -ENOBUFS) {
			err = cladefs_dax_throttle(cladefs_dax_of(inode), &throttles);
			if (err)
				return err;
			continue;
		}
		if (err != -EXDEV)
			return err;
		err = cladefs_dax_prepare_write_range(inode, pos, true);
		if (err)
			return err;
	}
}

/*
 * cladefs: as a file `old` bytes long grows to `size`, zeroes what its last page holds past `old` and
 * short of `size`: a mapping's stores there aren't the file's, and growing would have reads find
 * them (TASK-79.53). Here, not left to Fxfs, whose size may be past `old` as a failed write's is, and
 * which then zeroes from there. With the invalidate_lock held for writing.
 */
static int cladefs_dax_zero_tail(struct inode *inode, loff_t old, loff_t size)
{
	if (!offset_in_page(old))
		return 0;
	return cladefs_dax_zero_part(inode, old, min_t(loff_t, size, round_up(old, PAGE_SIZE)) - old);
}

/*
 * Readies the last block of `inode`, `old` bytes long, for Fxfs growing the file to `size` to copy
 * it, zeroed past `old`, through the device: what the window holds past the end zeroed, its stores
 * written back as the mapping is dropped, then sealed. The drop lets the copy's blocks show in
 * the window. Fxfs's growths zero the block so: a truncate's, and a clone's
 * (TASK-153.4). With the invalidate_lock held for writing.
 */
static int cladefs_dax_grow_begin(struct inode *inode, loff_t old, loff_t size)
{
	int err;

	if (size <= old || PAGE_ALIGNED(old))
		return 0;
	err = cladefs_dax_zero_tail(inode, old, size);
	if (!err)
		err = cladefs_dax_break_layouts(inode, round_down(old, PAGE_SIZE));
	if (!err)
		err = cladefs_dax_drop(inode, old >> CLADEFS_DAX_SHIFT, old >> CLADEFS_DAX_SHIFT, -1);
	if (!err)
		err = cladefs_nofs(cladefs_fsync(cladefs_vfs_fs(inode->i_sb)));
	return err;
}

/*
 * A reflink's `len` bytes at `pos` of `inode`, to or from them (cladefs_data_ops): their DAX entries
 * go, once their pages are unused, and the window's ranges of them, to be mapped again as they are
 * after the reflink, a range's dirty pages written back first (cladefs_dax_drop_pinned), as XFS writes
 * back both files' ranges: a destination's are other blocks. And the destination's ranges too
 * small for its new size, `size`, and its last block, which Fxfs zeroes past the end as it grows
 * (cladefs_dax_grow_begin).
 */
static int cladefs_dax_remap_begin(struct inode *inode, loff_t pos, loff_t len, loff_t size)
{
	int err;

	if (!cladefs_dax_file(inode) || !len)
		return 0;
	err = cladefs_dax_grow_begin(inode, i_size_read(inode), size);
	if (!err)
		err = cladefs_dax_break_layouts(inode, round_down(pos, PAGE_SIZE));
	if (!err)
		err = cladefs_dax_drop(inode, pos >> CLADEFS_DAX_SHIFT, (pos + len - 1) >> CLADEFS_DAX_SHIFT, -1);
	if (!err)
		err = cladefs_dax_fit(inode, size);
	return err;
}

/*
 * A hole punched from `start` to `end` (cladefs_data_ops): the pages' mappings go, once unused, from
 * the first, which its partial pages, zeroed through the window (cladefs_dax_zero_part), map again.
 */
static int cladefs_dax_punch_begin(struct inode *inode, loff_t start, loff_t end)
{
	return cladefs_dax_break_layouts(inode, round_down(start, PAGE_SIZE));
}

/*
 * The chunks of a hole's whole pages from `start` to `end`, before Fxfs frees their blocks: mapped
 * again from where their data is, if anywhere.
 */
static int cladefs_dax_punch_drop(struct inode *inode, loff_t start, loff_t end)
{
	return cladefs_dax_drop(inode, start >> CLADEFS_DAX_SHIFT, (end - 1) >> CLADEFS_DAX_SHIFT, -1);
}

/*
 * Before and after a change of size to `size` (cladefs_data_ops): the first waits for the pages past
 * the size to be unused, and zeroes the last page past it, holding the invalidate_lock and the
 * writeback lock, which the second lets go of, once it has mapped zeros past the size.
 */
static int cladefs_dax_resize_begin(struct inode *inode, loff_t size)
{
	struct cladefs_dax_file *file = cladefs_dax_file(inode);
	int err;

	filemap_invalidate_lock(inode->i_mapping);
	/*
	 * An ordinary grow zeroes from Fxfs's end, which is as writeback leaves it: what is dirty is
	 * written first, so that it is the inode's (doc-13).
	 */
	err = cladefs_dax_write_back(inode);
	if (err) {
		filemap_invalidate_unlock(inode->i_mapping);
		return err;
	}
	if (size >= i_size_read(inode)) {
		/* An ordinary grow copies and zeroes the last partial block through the device. */
		err = cladefs_dax_grow_begin(inode, i_size_read(inode), size);
		/* The ranges the file's end moves into, mapped as large as they need. */
		if (!err)
			err = cladefs_dax_fit(inode, size);
	} else {
		err = cladefs_dax_break_layouts(inode, round_up(size, PAGE_SIZE));
		/* What the last page holds past the end reads as zeros if the file grows again. */
		if (!err && offset_in_page(size))
			err = cladefs_dax_zero_part(inode, size, PAGE_SIZE - offset_in_page(size));
	}
	if (err) {
		filemap_invalidate_unlock(inode->i_mapping);
		return err;
	}
	/*
	 * cladefs: no writeback until cladefs_dax_resize_end: one that read the size before the change
	 * wrote the pages past the new end after Fxfs cut them, and grew the file again. Taken last,
	 * as zeroing above may wait for the budget's worker, which writes back.
	 */
	cladefs_nofs_mutex_lock(&file->wb_lock);
	return 0;
}

static void cladefs_dax_resize_end(struct inode *inode, loff_t size)
{
	struct cladefs_dax_file *file = cladefs_dax_file(inode);
	struct cladefs_dax *dax = cladefs_dax_of(inode);
	unsigned long first = DIV_ROUND_UP(size, PAGE_SIZE), page, chunk;
	struct cladefs_dax_range *range;
	int err;
	cladefs_nofs_scope();

	cladefs_nofs_down_write(&file->sem);
	xa_for_each_start(&file->ranges, chunk, range, size >> CLADEFS_DAX_SHIFT) {
		loff_t start = cladefs_dax_chunk_start(range);

		if (start >= round_up(size, PAGE_SIZE)) {
			cladefs_dax_unmap_range(dax, range);
			continue;
		}
		page = first - (start >> PAGE_SHIFT);
		if (page >= range->pages)
			continue;
		if (!cladefs_dax_pages_any_mapped(range->state, page, range->pages))
			continue;
		/*
		 * cladefs: the pages past the end map zeros, which every page of a range maps
		 * (cladefs_dax_map_pages), the MAP freeing their private pages, their dirty pages' room
		 * released; unmapped if it fails, as a write maps them again.
		 */
		if (!range->region) {
			trace_cladefs_dax_map(CLADEFS_TRACE_MAP_ZEROS, inode->i_ino,
					   cladefs_dax_page_offset(range, page),
					   (u64)(range->pages - page) << PAGE_SHIFT, 1);
			err = cladefs_device_map_zeros(dax->dev, cladefs_dax_page_offset(range, page),
						    (u64)(range->pages - page) << PAGE_SHIFT);
			if (!err) {
				cladefs_dax_dropped(dax, range,
						 cladefs_dax_pages_cut(range->state, page,
								    range->pages - page), true);
				continue;
			}
			pr_warn("cladefs: mapping zeros past ino %lu's end at %llx of the DAX window failed: %d\n",
				inode->i_ino, cladefs_dax_page_offset(range, page), err);
		}
		/* A slice's pages past the end stay its region's: the slice no longer maps them. */
		err = range->region ? 0 :
		      cladefs_device_unmap(dax->dev, range->window_offset + ((u64)page << PAGE_SHIFT),
					(u64)(range->pages - page) << PAGE_SHIFT);
		if (err)
			pr_warn("cladefs: unmapping the DAX window at %llx failed: %d\n",
				range->window_offset, err);
		/* cladefs: the UNMAP freed their private pages, unless it failed. */
		cladefs_dax_dropped(dax, range,
				 cladefs_dax_pages_unmap(range->state, page, range->pages - page), !err);
	}
	cladefs_nofs_up_write(&file->sem);
	cladefs_nofs_mutex_unlock(&file->wb_lock);
	filemap_invalidate_unlock(inode->i_mapping);
}

/*
 * cladefs: what a DAX file's data's changes do in the window (data.c): a cut's tail zeroed through it
 * (cladefs_dax_resize_begin), and so a punch's partial pages (cladefs_dax_zero_part).
 */
static const struct cladefs_data_ops cladefs_dax_data_ops = {
	.resize_begin = cladefs_dax_resize_begin,
	.resize_end = cladefs_dax_resize_end,
	.keeps_tail = true,
	.punch_begin = cladefs_dax_punch_begin,
	.zero = cladefs_dax_zero_part,
	.punch_drop = cladefs_dax_punch_drop,
	.remap_begin = cladefs_dax_remap_begin,
};
